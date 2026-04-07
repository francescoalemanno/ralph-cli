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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunnerInvocation {
    pub run_id: String,
    pub prompt_text: String,
    pub project_dir: Utf8PathBuf,
    pub run_dir: Utf8PathBuf,
    pub prompt_path: Utf8PathBuf,
    pub prompt_name: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunnerResult {
    pub output: String,
    pub exit_code: i32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowRunSummary {
    pub workflow_id: String,
    pub run_id: String,
    pub final_prompt_id: String,
    pub run_dir: Utf8PathBuf,
    pub workflow_path: Utf8PathBuf,
    pub status: LastRunStatus,
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

    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
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
        self.cancelled.load(Ordering::SeqCst)
    }
}

#[cfg(test)]
mod tests {
    use super::{LastRunStatus, RunControl};

    #[test]
    fn cancel_marks_control_as_canceled() {
        let control = RunControl::new();
        assert!(!control.is_cancelled());
        control.cancel();
        assert!(control.is_cancelled());
        control.cancel();
        assert!(control.is_cancelled());
    }

    #[test]
    fn stores_agent_override() {
        let control = RunControl::new();
        assert_eq!(control.agent_id(), None);
        control.set_agent_id("codex");
        assert_eq!(control.agent_id().as_deref(), Some("codex"));
    }

    #[test]
    fn status_labels_are_stable() {
        assert_eq!(LastRunStatus::Completed.label(), "completed");
    }
}
