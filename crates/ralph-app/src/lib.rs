mod console;
mod prompt;
mod workflow_run;

use anyhow::{Result, anyhow};
use async_trait::async_trait;
use camino::{Utf8Path, Utf8PathBuf};
use ralph_core::{
    AgentConfig, AppConfig, LastRunStatus, RunnerConfig, WorkflowDefinition, WorkflowSummary,
    list_workflows, load_workflow,
};
use ralph_runner::{
    CommandRunner, InteractiveSessionInvocation, InteractiveSessionOutcome, RunnerAdapter,
};

pub use console::ConsoleDelegate;
pub use workflow_run::{WorkflowRequestInput, WorkflowRunInput};

#[derive(Debug)]
pub enum RunEvent {
    IterationStarted {
        prompt_name: String,
        iteration: usize,
        max_iterations: usize,
    },
    Output(String),
    ParallelWorkerLaunched {
        channel_id: String,
        label: String,
    },
    ParallelWorkerStarted {
        channel_id: String,
        label: String,
    },
    ParallelWorkerFinished {
        channel_id: String,
        label: String,
        exit_code: i32,
    },
    Note(String),
    Finished {
        status: LastRunStatus,
        summary: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanningQuestion {
    pub question: String,
    pub options: Vec<String>,
    pub context: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanningAnswerSource {
    Option,
    Custom,
}

impl PlanningAnswerSource {
    pub fn label(self) -> &'static str {
        match self {
            Self::Option => "option",
            Self::Custom => "custom",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanningQuestionAnswer {
    pub answer: String,
    pub source: PlanningAnswerSource,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanningDraftReview {
    pub target_path: Utf8PathBuf,
    pub draft: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanningDraftDecisionKind {
    Accept,
    Revise,
    Reject,
}

impl PlanningDraftDecisionKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Accept => "accept",
            Self::Revise => "revise",
            Self::Reject => "reject",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanningDraftDecision {
    pub kind: PlanningDraftDecisionKind,
    pub feedback: Option<String>,
}

pub fn format_iteration_banner(
    prompt_name: &str,
    iteration: usize,
    max_iterations: usize,
) -> String {
    let title = format!(
        " {} ITERATION {}/{} ",
        prompt_name, iteration, max_iterations
    );
    let width = title.len().max(44);
    let rule = "=".repeat(width);
    format!("\n{rule}\n{title:=^width$}\n{rule}", width = width)
}

#[async_trait]
pub trait RunDelegate: Send {
    async fn on_event(&mut self, event: RunEvent) -> Result<()>;

    async fn answer_planning_question(
        &mut self,
        _question: &PlanningQuestion,
    ) -> Result<PlanningQuestionAnswer> {
        Err(anyhow!(
            "planning questions are not supported by this run delegate"
        ))
    }

    async fn review_planning_draft(
        &mut self,
        _draft: &PlanningDraftReview,
    ) -> Result<PlanningDraftDecision> {
        Err(anyhow!(
            "planning draft review is not supported by this run delegate"
        ))
    }

    async fn run_interactive_session(
        &mut self,
        _config: &RunnerConfig,
        _invocation: &InteractiveSessionInvocation,
    ) -> Result<Option<InteractiveSessionOutcome>> {
        Ok(None)
    }
}

#[derive(Debug, Clone)]
pub struct RalphApp<R = CommandRunner> {
    project_dir: Utf8PathBuf,
    config: AppConfig,
    runner: R,
}

impl RalphApp<CommandRunner> {
    pub fn load(project_dir: impl Into<Utf8PathBuf>) -> Result<Self> {
        let project_dir = project_dir.into();
        let config = AppConfig::load(&project_dir)?;
        Ok(Self {
            project_dir,
            config,
            runner: CommandRunner,
        })
    }
}

impl<R> RalphApp<R> {
    #[cfg(test)]
    pub(crate) fn new(project_dir: Utf8PathBuf, config: AppConfig, runner: R) -> Self {
        Self {
            project_dir,
            config,
            runner,
        }
    }

    pub fn config(&self) -> &AppConfig {
        &self.config
    }

    pub fn config_mut(&mut self) -> &mut AppConfig {
        &mut self.config
    }

    pub fn agent_id(&self) -> &str {
        self.config.agent_id()
    }

    pub fn agent_name(&self) -> String {
        self.config.agent_name()
    }

    pub fn available_agents(&self) -> Vec<&AgentConfig> {
        self.config.available_agents()
    }

    pub fn all_agents(&self) -> &[AgentConfig] {
        self.config.all_agents()
    }

    pub fn set_agent(&mut self, agent_id: &str) -> Result<()> {
        if self.config.agent_definition(agent_id).is_none() {
            return Err(anyhow!("agent '{}' is not defined", agent_id));
        }
        self.config.set_agent(agent_id);
        Ok(())
    }

    pub fn persist_agent(&mut self, agent_id: &str) -> Result<()> {
        AppConfig::persist_scoped_coding_agent(
            &self.project_dir,
            ralph_core::ConfigFileScope::Project,
            agent_id,
        )?;
        self.config.set_agent(agent_id);
        Ok(())
    }

    pub fn project_dir(&self) -> &Utf8Path {
        &self.project_dir
    }

    pub fn list_workflows(&self) -> Result<Vec<WorkflowSummary>> {
        list_workflows()
    }

    pub fn load_workflow(&self, workflow_id: &str) -> Result<WorkflowDefinition> {
        load_workflow(workflow_id)
    }

    pub fn resolve_workflow_edit_path(&self, workflow_id: &str) -> Result<Utf8PathBuf> {
        self.load_workflow(workflow_id)?
            .source_path()
            .map(Utf8Path::to_path_buf)
            .ok_or_else(|| anyhow!("workflow '{}' does not have a source path", workflow_id))
    }

    pub fn read_utf8_file(&self, path: &Utf8Path) -> Result<String> {
        std::fs::read_to_string(path).map_err(|error| anyhow!("failed to read {}: {error}", path))
    }
}

impl<R> RalphApp<R>
where
    R: RunnerAdapter,
{
    pub fn run_interactive_session_with_config(
        &self,
        config: &RunnerConfig,
        invocation: &InteractiveSessionInvocation,
    ) -> Result<InteractiveSessionOutcome> {
        self.runner.run_interactive_session(config, invocation)
    }
}
