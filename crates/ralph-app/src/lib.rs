mod console;
mod engine;
mod interactive;
mod prompt;
mod prompt_run;
mod run;
mod workflow;
mod workflow_run;

use anyhow::{Result, anyhow};
use async_trait::async_trait;
use camino::{Utf8Path, Utf8PathBuf};
use ralph_core::{
    AgentConfig, AppConfig, LastRunStatus, PromptFile, ScaffoldId, TargetReview, TargetStore,
    TargetSummary,
};
use ralph_runner::CommandRunner;
use workflow::PLAN_DRIVEN_GOAL_FILE;

pub use console::ConsoleDelegate;
pub use workflow::{
    WorkflowAction, WorkflowDerivedState, WorkflowKind, WorkflowRunAdvice, WorkflowStatus,
};

#[derive(Debug, Clone)]
pub enum RunEvent {
    IterationStarted {
        prompt_name: String,
        iteration: usize,
        max_iterations: usize,
    },
    Output(String),
    Note(String),
    Finished {
        status: LastRunStatus,
        summary: String,
    },
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
}

#[derive(Debug, Clone)]
pub struct RalphApp<R = CommandRunner> {
    project_dir: Utf8PathBuf,
    store: TargetStore,
    config: AppConfig,
    runner: R,
}

impl RalphApp<CommandRunner> {
    pub fn load(project_dir: impl Into<Utf8PathBuf>) -> Result<Self> {
        let project_dir = project_dir.into();
        let config = AppConfig::load(&project_dir)?;
        Ok(Self {
            store: TargetStore::new(project_dir.clone()),
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
            store: TargetStore::new(project_dir.clone()),
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

    pub fn list_targets(&self) -> Result<Vec<TargetSummary>> {
        self.store.list_targets()
    }

    pub fn create_target(
        &self,
        target_id: &str,
        scaffold: Option<ScaffoldId>,
    ) -> Result<TargetSummary> {
        self.store.create_target(target_id, scaffold)
    }

    pub fn review_target(&self, target: &str) -> Result<TargetReview> {
        self.store.review_target(target)
    }

    pub fn delete_target(&self, target: &str) -> Result<()> {
        self.store.delete_target(target)
    }

    pub fn resolve_target_edit_path(
        &self,
        target: &str,
        requested_file: Option<&str>,
    ) -> Result<Utf8PathBuf> {
        let config = self.store.read_target_config(target)?;
        let target_summary = self.store.load_target(target)?;
        let target_dir = self.store.target_paths(target)?.dir;

        if !config.entrypoints.is_empty() {
            let entrypoints = crate::engine::resolve_target_entrypoints(&config, &target_summary);
            if let Some(entrypoint) =
                crate::engine::resolve_default_entrypoint(&config, &entrypoints)
                && let Some(edit_path) = entrypoint.edit_path()
            {
                let resolved =
                    crate::engine::resolve_artifact_path(&self.project_dir, &target_dir, edit_path);
                return match requested_file {
                    None => Ok(resolved),
                    Some(name)
                        if name == edit_path
                            || name == resolved.file_name().unwrap_or_default() =>
                    {
                        Ok(resolved)
                    }
                    Some(name) => Err(anyhow!(
                        "entrypoint '{}' exposes '{}' for editing, got '{}'",
                        entrypoint.id(),
                        edit_path,
                        name
                    )),
                };
            }
        }

        if config.uses_hidden_workflow() {
            return match requested_file {
                None | Some(PLAN_DRIVEN_GOAL_FILE) => Ok(target_dir.join(PLAN_DRIVEN_GOAL_FILE)),
                Some(name) => Err(anyhow!(
                    "workflow targets only expose {PLAN_DRIVEN_GOAL_FILE} for editing, got '{name}'"
                )),
            };
        }

        Ok(self.resolve_prompt(target, requested_file)?.path)
    }

    fn resolve_prompt(&self, target: &str, prompt_name: Option<&str>) -> Result<PromptFile> {
        let target_summary = self.store.load_target(target)?;
        self.select_prompt(&target_summary, prompt_name)
    }

    fn select_prompt(
        &self,
        target_summary: &TargetSummary,
        prompt_name: Option<&str>,
    ) -> Result<PromptFile> {
        if target_summary.uses_hidden_workflow() {
            return Err(anyhow!(
                "target '{}' uses a workflow mode and does not expose runnable prompts",
                target_summary.id
            ));
        }

        if target_summary.prompt_files.is_empty() {
            return Err(anyhow!(
                "target '{}' has no runnable prompt files",
                target_summary.id
            ));
        }

        if let Some(prompt_name) = prompt_name {
            return target_summary
                .prompt_files
                .iter()
                .find(|prompt| prompt.name == prompt_name)
                .cloned()
                .ok_or_else(|| {
                    anyhow!(
                        "prompt '{prompt_name}' does not exist for '{}'",
                        target_summary.id
                    )
                });
        }

        if let Some(last_prompt) = &target_summary.last_prompt
            && let Some(prompt) = target_summary
                .prompt_files
                .iter()
                .find(|prompt| &prompt.name == last_prompt)
        {
            return Ok(prompt.clone());
        }

        if let Some(prompt) = target_summary
            .prompt_files
            .iter()
            .find(|prompt| prompt.name == "prompt_main.md")
        {
            return Ok(prompt.clone());
        }

        if target_summary.prompt_files.len() == 1 {
            return Ok(target_summary.prompt_files[0].clone());
        }

        let available = target_summary
            .prompt_files
            .iter()
            .map(|prompt| prompt.name.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        Err(anyhow!(
            "target '{}' has multiple prompt files; choose one with --prompt ({available})",
            target_summary.id
        ))
    }
}
