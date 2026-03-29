use std::sync::{
    Arc,
    atomic::{AtomicU8, Ordering},
    Mutex,
};

use camino::Utf8PathBuf;
use serde::{Deserialize, Serialize};

use crate::config::CodingAgent;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowState {
    Empty,
    Planned,
    Completed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunnerMode {
    Plan,
    Build,
}

impl RunnerMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Plan => "plan",
            Self::Build => "build",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpecPaths {
    pub spec_path: Utf8PathBuf,
    pub progress_path: Utf8PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpecSummary {
    pub spec_path: Utf8PathBuf,
    pub progress_path: Utf8PathBuf,
    pub state: WorkflowState,
    pub spec_preview: String,
    pub progress_preview: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewData {
    pub spec_path: Utf8PathBuf,
    pub progress_path: Utf8PathBuf,
    pub spec_contents: String,
    pub progress_contents: String,
    pub state: WorkflowState,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClarificationOption {
    pub label: String,
    pub description: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClarificationRequest {
    pub question: String,
    #[serde(default)]
    pub options: Vec<ClarificationOption>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClarificationExchange {
    pub question: String,
    pub answer: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunnerInvocation {
    pub prompt_text: String,
    pub project_dir: Utf8PathBuf,
    pub mode: RunnerMode,
    pub spec_path: Utf8PathBuf,
    pub progress_path: Utf8PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunnerResult {
    pub stdout: String,
    pub stderr: String,
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
        *self.coding_agent.lock().expect("run control mutex poisoned") = Some(agent);
    }

    pub fn coding_agent(&self) -> Option<CodingAgent> {
        *self.coding_agent.lock().expect("run control mutex poisoned")
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
    use super::RunControl;
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
}
