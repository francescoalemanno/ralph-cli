mod artifact_templates;
mod clarification;
mod config;
mod markers;
mod prompts;
mod slug;
mod store;
mod types;

pub use artifact_templates::{
    REQUIRED_SPEC_HEADINGS, default_feedback_contents, default_progress_contents,
    empty_spec_contents, initial_spec_contents, required_spec_format_outline, sample_spec_contents,
};
pub use clarification::parse_clarification_request;
pub use config::{
    AppConfig, CliColorMode, CliConfig, CliOutputMode, CliPagerMode, CliPromptInputMode,
    CodingAgent, ConfigFileScope, PromptTransport, QuestionSupportMode, RunnerConfig, ThemeConfig,
};
pub use markers::{
    BuilderMarker, MarkerError, PlanningMarker, append_persisted_done_marker,
    parse_builder_marker_from_output, parse_planning_marker_from_output,
    strip_persisted_promise_markers,
};
pub use prompts::{
    BuildPromptContext, PlanningPromptContext, ProgressRevisionPromptContext, build_prompt,
    planning_prompt, progress_revision_prompt,
};
pub use slug::generate_slug;
pub use store::ArtifactStore;
pub use types::{
    ClarificationAnswer, ClarificationOption, ClarificationRequest, ReviewData, RunControl,
    RunnerInvocation, RunnerMode, RunnerResult, SpecPaths, SpecSummary, WorkflowState,
};
