mod agent;
mod atomic;
mod config;
mod scaffold;
mod store;
mod types;

pub use agent::{AgentConfig, CodingAgent, CommandMode, PromptInput, RunnerConfig, builtin_agents};
pub use atomic::atomic_write;
pub use config::{AppConfig, ConfigFileScope, ThemeConfig};
pub use scaffold::bare_prompt_template;
pub use store::TargetStore;
pub use types::{
    LastRunStatus, PlanDrivenInflight, PlanDrivenPhase, PlanDrivenWorkflowState, PromptFile,
    RunControl, RunnerInvocation, RunnerResult, ScaffoldId, TargetConfig, TargetFile,
    TargetFileContents, TargetPaths, TargetReview, TargetSummary, WorkflowMode,
};
