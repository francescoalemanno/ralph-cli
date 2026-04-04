use std::{
    collections::BTreeMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU8, Ordering},
    },
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
    TaskDriven,
    PlanDriven,
}

impl ScaffoldId {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SinglePrompt => "single_prompt",
            Self::PlanBuild => "plan_build",
            Self::TaskDriven => "task_driven",
            Self::PlanDriven => "plan_driven",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowMode {
    TaskDriven,
    PlanDriven,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntrypointKind {
    Prompt,
    Flow,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TargetEntrypoint {
    Prompt {
        id: String,
        path: String,
        #[serde(default)]
        hidden: bool,
        #[serde(default)]
        edit_path: Option<String>,
    },
    Flow {
        id: String,
        flow: String,
        #[serde(default)]
        params: BTreeMap<String, String>,
        #[serde(default)]
        hidden: bool,
        #[serde(default)]
        edit_path: Option<String>,
    },
}

impl TargetEntrypoint {
    pub fn id(&self) -> &str {
        match self {
            Self::Prompt { id, .. } | Self::Flow { id, .. } => id,
        }
    }

    pub fn kind(&self) -> EntrypointKind {
        match self {
            Self::Prompt { .. } => EntrypointKind::Prompt,
            Self::Flow { .. } => EntrypointKind::Flow,
        }
    }

    pub fn hidden(&self) -> bool {
        match self {
            Self::Prompt { hidden, .. } | Self::Flow { hidden, .. } => *hidden,
        }
    }

    pub fn edit_path(&self) -> Option<&str> {
        match self {
            Self::Prompt { edit_path, .. } | Self::Flow { edit_path, .. } => edit_path.as_deref(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct FlowRuntimeState {
    #[serde(default)]
    pub active_entrypoint: Option<String>,
    #[serde(default)]
    pub current_node: Option<String>,
    #[serde(default)]
    pub vars: BTreeMap<String, String>,
    #[serde(default)]
    pub last_signal: Option<String>,
    #[serde(default)]
    pub last_note: Option<String>,
    #[serde(default)]
    pub inflight: Option<FlowRuntimeInflight>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FlowRuntimeInflight {
    pub node_id: String,
    pub started_at: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum PlanDrivenPhase {
    #[default]
    Plan,
    Build,
    Paused,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct PlanDrivenWorkflowState {
    #[serde(default)]
    pub phase: PlanDrivenPhase,
    #[serde(default)]
    pub last_goal_hash: Option<String>,
    #[serde(default)]
    pub last_content_hash: Option<String>,
    #[serde(default)]
    pub last_planned_at: Option<u64>,
    #[serde(default)]
    pub last_built_at: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanDrivenInflight {
    pub phase: PlanDrivenPhase,
    pub goal_hash: String,
    pub content_hash: String,
    pub started_at: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TargetConfig {
    pub id: String,
    #[serde(default)]
    pub scaffold: Option<ScaffoldId>,
    #[serde(default)]
    pub default_entrypoint: Option<String>,
    #[serde(default)]
    pub entrypoints: Vec<TargetEntrypoint>,
    #[serde(default)]
    pub runtime: Option<FlowRuntimeState>,
    #[serde(default)]
    pub mode: Option<WorkflowMode>,
    #[serde(default)]
    pub workflow: Option<PlanDrivenWorkflowState>,
    #[serde(default)]
    pub inflight: Option<PlanDrivenInflight>,
    #[serde(default)]
    pub created_at: Option<u64>,
    #[serde(default)]
    pub max_iterations: Option<usize>,
    #[serde(default)]
    pub last_prompt: Option<String>,
    #[serde(default)]
    pub last_run_status: LastRunStatus,
}

impl TargetConfig {
    pub fn uses_hidden_workflow(&self) -> bool {
        self.entrypoints
            .iter()
            .any(|entrypoint| entrypoint.kind() == EntrypointKind::Flow)
            || matches!(
                self.mode,
                Some(WorkflowMode::TaskDriven | WorkflowMode::PlanDriven)
            )
    }
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
    #[serde(default)]
    pub default_entrypoint: Option<String>,
    #[serde(default)]
    pub flow_entrypoints: Vec<String>,
    pub mode: Option<WorkflowMode>,
    pub created_at: Option<u64>,
    pub last_prompt: Option<String>,
    pub last_run_status: LastRunStatus,
}

impl TargetSummary {
    pub fn uses_hidden_workflow(&self) -> bool {
        !self.flow_entrypoints.is_empty()
            || matches!(
                self.mode,
                Some(WorkflowMode::TaskDriven | WorkflowMode::PlanDriven)
            )
    }
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
    agent_id: Arc<Mutex<Option<String>>>,
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
    use super::{LastRunStatus, RunControl, ScaffoldId, TargetConfig, TargetSummary, WorkflowMode};

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
        assert_eq!(ScaffoldId::TaskDriven.as_str(), "task_driven");
        assert_eq!(ScaffoldId::PlanDriven.as_str(), "plan_driven");
    }

    #[test]
    fn workflow_visibility_comes_from_mode() {
        let config = TargetConfig {
            id: "demo".to_owned(),
            scaffold: None,
            default_entrypoint: None,
            entrypoints: Vec::new(),
            runtime: None,
            mode: Some(WorkflowMode::PlanDriven),
            workflow: None,
            inflight: None,
            created_at: None,
            max_iterations: None,
            last_prompt: None,
            last_run_status: LastRunStatus::NeverRun,
        };
        let summary = TargetSummary {
            id: "demo".to_owned(),
            dir: camino::Utf8PathBuf::from("/tmp/demo"),
            prompt_files: Vec::new(),
            files: Vec::new(),
            scaffold: None,
            default_entrypoint: None,
            flow_entrypoints: Vec::new(),
            mode: Some(WorkflowMode::TaskDriven),
            created_at: None,
            last_prompt: None,
            last_run_status: LastRunStatus::NeverRun,
        };

        assert!(config.uses_hidden_workflow());
        assert!(summary.uses_hidden_workflow());
    }
}
