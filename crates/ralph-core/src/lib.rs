mod config;
mod slug;
mod store;
mod types;

pub use config::{
    AppConfig, CliColorMode, CliConfig, CliOutputMode, CliPagerMode, CliPromptInputMode,
    CodingAgent, ConfigFileScope, PromptTransport, RunnerConfig, ThemeConfig,
};
pub use slug::generate_slug;
pub use store::{
    ARTIFACT_DIR_NAME, TARGETS_DIR_NAME, TargetStore, bare_prompt_template, is_prompt_file_name,
};
pub use types::{
    LastRunStatus, PromptFile, RunControl, RunnerInvocation, RunnerResult, ScaffoldId,
    TargetConfig, TargetFile, TargetFileContents, TargetPaths, TargetReview, TargetSummary,
};
