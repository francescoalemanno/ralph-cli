use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};

use camino::Utf8PathBuf;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum LastRunStatus {
    #[default]
    NeverRun,
    Completed,
    MaxIterations,
    Failed,
    Canceled,
}

impl LastRunStatus {
    pub fn label(self) -> &'static str {
        match self {
            Self::NeverRun => "never_run",
            Self::Completed => "completed",
            Self::MaxIterations => "max_iterations",
            Self::Failed => "failed",
            Self::Canceled => "canceled",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ScaffoldId {
    #[default]
    SinglePrompt,
    PlanBuild,
}

impl ScaffoldId {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SinglePrompt => "single_prompt",
            Self::PlanBuild => "plan_build",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TargetConfig {
    pub id: String,
    #[serde(default)]
    pub scaffold: Option<ScaffoldId>,
    #[serde(default)]
    pub created_at: Option<u64>,
    #[serde(default)]
    pub max_iterations: Option<usize>,
    #[serde(default)]
    pub last_prompt: Option<String>,
    #[serde(default)]
    pub last_run_status: LastRunStatus,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TargetPaths {
    pub dir: Utf8PathBuf,
    pub config_path: Utf8PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromptFile {
    pub name: String,
    pub path: Utf8PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TargetFile {
    pub name: String,
    pub path: Utf8PathBuf,
    pub is_prompt: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TargetSummary {
    pub id: String,
    pub dir: Utf8PathBuf,
    pub prompt_files: Vec<PromptFile>,
    pub files: Vec<TargetFile>,
    pub scaffold: Option<ScaffoldId>,
    pub created_at: Option<u64>,
    pub last_prompt: Option<String>,
    pub last_run_status: LastRunStatus,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TargetReview {
    pub summary: TargetSummary,
    pub files: Vec<TargetFileContents>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TargetFileContents {
    pub name: String,
    pub path: Utf8PathBuf,
    pub contents: String,
    pub is_prompt: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunnerInvocation {
    pub prompt_text: String,
    pub project_dir: Utf8PathBuf,
    pub target_dir: Utf8PathBuf,
    pub prompt_path: Utf8PathBuf,
    pub prompt_name: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunnerResult {
    pub output: String,
    pub exit_code: i32,
}

#[derive(Debug, Clone, Default)]
pub struct RunControl {
    cancelled: Arc<AtomicBool>,
    agent_id: Arc<Mutex<Option<String>>>,
}

impl RunControl {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) -> u8 {
        self.cancelled.store(true, Ordering::SeqCst);
        1
    }

    pub fn force_cancel(&self) -> u8 {
        self.cancel()
    }

    pub fn set_agent_id(&self, agent_id: impl Into<String>) {
        *self.agent_id.lock().expect("run control mutex poisoned") = Some(agent_id.into());
    }

    pub fn agent_id(&self) -> Option<String> {
        self.agent_id
            .lock()
            .expect("run control mutex poisoned")
            .clone()
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancel_stage() >= 1
    }

    pub fn is_force_cancelled(&self) -> bool {
        self.is_cancelled()
    }

    pub fn cancel_stage(&self) -> u8 {
        u8::from(self.cancelled.load(Ordering::SeqCst))
    }
}

#[cfg(test)]
mod tests {
    use super::{LastRunStatus, RunControl, ScaffoldId};

    #[test]
    fn cancellation_escalates_and_saturates() {
        let control = RunControl::new();
        assert_eq!(control.cancel_stage(), 0);
        assert_eq!(control.cancel(), 1);
        assert!(control.is_cancelled());
        assert!(control.is_force_cancelled());
        assert_eq!(control.cancel(), 1);
        assert_eq!(control.cancel_stage(), 1);
        assert_eq!(control.force_cancel(), 1);
        assert_eq!(control.cancel_stage(), 1);
    }

    #[test]
    fn stores_agent_override() {
        let control = RunControl::new();
        assert_eq!(control.agent_id(), None);
        control.set_agent_id("codex");
        assert_eq!(control.agent_id().as_deref(), Some("codex"));
    }

    #[test]
    fn status_and_scaffold_have_stable_labels() {
        assert_eq!(LastRunStatus::Completed.label(), "completed");
        assert_eq!(ScaffoldId::SinglePrompt.as_str(), "single_prompt");
        assert_eq!(ScaffoldId::PlanBuild.as_str(), "plan_build");
    }
}
