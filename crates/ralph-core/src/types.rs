use std::sync::{
    Arc, Mutex,
    atomic::{AtomicU8, Ordering},
};

use camino::Utf8PathBuf;
use serde::{Deserialize, Serialize};

use crate::config::CodingAgent;

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
    Blank,
    Playbook,
}

impl ScaffoldId {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Blank => "blank",
            Self::Playbook => "playbook",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TargetConfig {
    pub id: String,
    #[serde(default)]
    pub scaffold: Option<ScaffoldId>,
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
    cancel_stage: Arc<AtomicU8>,
    coding_agent: Arc<Mutex<Option<CodingAgent>>>,
}

impl RunControl {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) -> u8 {
        self.bump_cancel_stage()
    }

    pub fn force_cancel(&self) -> u8 {
        self.cancel_stage.store(2, Ordering::SeqCst);
        2
    }

    pub fn set_coding_agent(&self, agent: CodingAgent) {
        *self
            .coding_agent
            .lock()
            .expect("run control mutex poisoned") = Some(agent);
    }

    pub fn coding_agent(&self) -> Option<CodingAgent> {
        *self
            .coding_agent
            .lock()
            .expect("run control mutex poisoned")
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancel_stage() >= 1
    }

    pub fn is_force_cancelled(&self) -> bool {
        self.cancel_stage() >= 2
    }

    pub fn cancel_stage(&self) -> u8 {
        self.cancel_stage.load(Ordering::SeqCst)
    }

    fn bump_cancel_stage(&self) -> u8 {
        let mut current = self.cancel_stage.load(Ordering::SeqCst);
        loop {
            let next = current.saturating_add(1).min(2);
            match self.cancel_stage.compare_exchange(
                current,
                next,
                Ordering::SeqCst,
                Ordering::SeqCst,
            ) {
                Ok(_) => return next,
                Err(observed) => current = observed,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{LastRunStatus, RunControl, ScaffoldId};
    use crate::CodingAgent;

    #[test]
    fn cancellation_escalates_and_saturates() {
        let control = RunControl::new();
        assert_eq!(control.cancel_stage(), 0);
        assert_eq!(control.cancel(), 1);
        assert!(control.is_cancelled());
        assert!(!control.is_force_cancelled());
        assert_eq!(control.cancel(), 2);
        assert!(control.is_force_cancelled());
        assert_eq!(control.cancel(), 2);
        assert_eq!(control.cancel_stage(), 2);
        assert_eq!(control.force_cancel(), 2);
        assert_eq!(control.cancel_stage(), 2);
    }

    #[test]
    fn stores_coding_agent_override() {
        let control = RunControl::new();
        assert_eq!(control.coding_agent(), None);
        control.set_coding_agent(CodingAgent::Codex);
        assert_eq!(control.coding_agent(), Some(CodingAgent::Codex));
    }

    #[test]
    fn status_and_scaffold_have_stable_labels() {
        assert_eq!(LastRunStatus::Completed.label(), "completed");
        assert_eq!(ScaffoldId::Playbook.as_str(), "playbook");
    }
}
