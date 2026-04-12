mod agent;
mod agent_events;
mod atomic;
mod config;
mod protocol;
mod terminal;
mod theme;
mod types;
mod workflow;

pub use agent::{AgentConfig, CodingAgent, CommandMode, PromptInput, RunnerConfig, builtin_agents};
pub use agent_events::{
    AGENT_EVENTS_WAL_FILE_NAME, AgentEventLogRead, AgentEventRecord, LoopControlDecision,
    MAIN_CHANNEL_ID, ParsedAgentEvent, RUNTIME_DIR_NAME, agent_events_wal_path, append_agent_event,
    append_agent_event_to_wal_path, current_agent_events_offset, latest_agent_event_body_from_wal,
    latest_agent_event_body_from_wal_in_channel, read_agent_events_since,
    read_agent_events_since_path, reduce_loop_control, validate_agent_event,
};
pub use atomic::atomic_write;
pub use config::{
    ARTIFACT_DIR_NAME, AppConfig, ConfigFileScope, ScopedGlobalConfigDirOverride, ThemeConfig,
    global_config_dir, scoped_global_config_dir_override,
};
pub use protocol::{
    HOST_CHANNEL_ID, PLANNING_ANSWER_EVENT, PLANNING_PLAN_FILE_EVENT, PLANNING_PROGRESS_EVENT,
    PLANNING_QUESTION_EVENT, PLANNING_REVIEW_EVENT, PLANNING_TARGET_PATH_EVENT,
    current_unix_timestamp_ms, format_timeout_duration,
};
pub use terminal::{AnsiStyle, TerminalTheme};
pub use theme::{ResolvedTheme, ThemeColor, ThemeMode, ThemeVariant};
pub use types::{LastRunStatus, RunControl, RunnerInvocation, RunnerResult, WorkflowRunSummary};
pub use workflow::{
    NO_ROUTE_ERROR, NO_ROUTE_OK, WorkflowDefinition, WorkflowFileRequest, WorkflowOptionDefinition,
    WorkflowParallelDefinition, WorkflowParallelJoin, WorkflowParallelWorkerDefinition,
    WorkflowPromptDefinition, WorkflowRequestDefinition, WorkflowRuntimeRequest, WorkflowSummary,
    WorkflowTransitionGuard, WorkflowTransitionGuardFailure, WorkflowTransitionGuardFailureAction,
    is_protected_builtin_workflow, list_all_workflows, list_workflows, load_workflow,
    load_workflow_from_path, seed_builtin_workflows_if_missing, workflow_config_dir,
    workflow_option_flag,
};
