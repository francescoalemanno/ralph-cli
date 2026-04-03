mod atomic;
mod config;
mod scaffold;
mod slug;
mod store;
mod types;

pub use atomic::atomic_write;
pub use config::{
    AppConfig, CliColorMode, CliConfig, CliOutputMode, CliPagerMode, CliPromptInputMode,
    CodingAgent, ConfigFileScope, PromptTransport, RunnerConfig, ThemeConfig,
};
pub use scaffold::bare_prompt_template;
pub use slug::generate_slug;
pub use store::{ARTIFACT_DIR_NAME, TARGETS_DIR_NAME, TargetStore, is_prompt_file_name};
pub use types::{
    GoalDrivenInflight, GoalDrivenPhase, GoalDrivenWorkflowState, LastRunStatus, PromptFile,
    RunControl, RunnerInvocation, RunnerResult, ScaffoldId, TargetConfig, TargetFile,
    TargetFileContents, TargetPaths, TargetReview, TargetSummary, WorkflowMode,
};
