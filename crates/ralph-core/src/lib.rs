mod agent;
mod agent_events;
mod atomic;
mod config;
mod scaffold;
mod store;
mod types;

pub use agent::{AgentConfig, CodingAgent, CommandMode, PromptInput, RunnerConfig, builtin_agents};
pub use agent_events::{
    AGENT_EVENTS_WAL_FILE_NAME, AgentEventLogRead, AgentEventRecord, LoopControlDecision,
    RUNTIME_DIR_NAME, agent_events_wal_path, append_agent_event, current_agent_events_offset,
    read_agent_events_since, reduce_loop_control,
};
pub use atomic::atomic_write;
pub use config::{AppConfig, ConfigFileScope, ThemeConfig};
pub use scaffold::bare_prompt_template;
pub use store::{TargetStore, list_prompt_names_in_dir};
pub use types::{
    LastRunStatus, PromptFile, RunControl, RunnerInvocation, RunnerResult, ScaffoldId,
    TargetConfig, TargetFile, TargetFileContents, TargetPaths, TargetReview, TargetSummary,
};
