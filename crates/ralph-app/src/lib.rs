use std::{
    fs,
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use camino::{Utf8Path, Utf8PathBuf};
use ralph_core::{
    AppConfig, CodingAgent, GoalDrivenInflight, GoalDrivenPhase, GoalDrivenWorkflowState,
    LastRunStatus, RunControl, RunnerConfig, RunnerInvocation, ScaffoldId, TargetConfig,
    TargetReview, TargetStore, TargetSummary, WorkflowMode,
};
use ralph_runner::{CommandRunner, RunnerAdapter, RunnerStreamEvent};
use serde::Deserialize;
use sha2::{Digest, Sha256};
const RALPH_ENV_PROJECT_DIR: &str = "{ralph-env:PROJECT_DIR}";
const RALPH_ENV_TARGET_DIR: &str = "{ralph-env:TARGET_DIR}";
const RALPH_ENV_PROMPT_PATH: &str = "{ralph-env:PROMPT_PATH}";
const RALPH_ENV_PROMPT_NAME: &str = "{ralph-env:PROMPT_NAME}";
const GOAL_DRIVEN_GOAL_FILE: &str = "GOAL.md";
const GOAL_DRIVEN_PLAN_FILE: &str = "plan.toml";
const GOAL_DRIVEN_SPECS_DIR: &str = "specs";
const GOAL_DRIVEN_PLAN_PROMPT: &str = "goal_driven_plan";
const GOAL_DRIVEN_BUILD_PROMPT: &str = "goal_driven_build";
const GOAL_DRIVEN_PAUSED_PROMPT: &str = "goal_driven_paused";
const TASK_BASED_PROGRESS_FILE: &str = "progress.toml";
const TASK_BASED_JOURNAL_FILE: &str = "journal.txt";
const TASK_BASED_BUILD_PROMPT: &str = "task_based_build";
const TASK_BASED_PAUSED_PROMPT: &str = "task_based_paused";

#[derive(Debug, Clone, PartialEq, Eq)]
struct ParsedPrompt {
    prompt_text: String,
    completion_criteria: Vec<CompletionCriterion>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum CompletionCriterion {
    Watch { path: String },
    NoLineContainsAll { path: String, tokens: Vec<String> },
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "ralph", rename_all = "snake_case")]
enum PromptDirective {
    Watch {
        path: String,
    },
    CompleteWhen {
        #[serde(rename = "type")]
        kind: CompletionDirectiveType,
        path: String,
        tokens: Vec<String>,
    },
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "snake_case")]
enum CompletionDirectiveType {
    NoLineContainsAll,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PreparedPromptRun {
    prompt_path: Utf8PathBuf,
    prompt_name: String,
    target_dir: Utf8PathBuf,
    raw_prompt: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GoalDrivenAction {
    Plan,
    Build,
    Paused,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct GoalDrivenHashes {
    goal_hash: String,
    content_hash: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TaskBasedHashes {
    goal_hash: String,
    content_hash: String,
}

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

impl<R> RalphApp<R>
where
    R: RunnerAdapter,
{
    pub fn new(project_dir: Utf8PathBuf, config: AppConfig, runner: R) -> Self {
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

    pub fn coding_agent(&self) -> CodingAgent {
        self.config.coding_agent()
    }

    pub fn set_coding_agent(&mut self, agent: CodingAgent) {
        self.config.set_coding_agent(agent);
    }

    pub fn persist_coding_agent(&mut self, agent: CodingAgent) -> Result<()> {
        AppConfig::persist_project_coding_agent(&self.project_dir, agent)?;
        self.config.set_coding_agent(agent);
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
        let target_dir = self.store.target_paths(target)?.dir;

        if matches!(
            config.mode,
            Some(WorkflowMode::GoalDriven | WorkflowMode::TaskBased)
        ) {
            return match requested_file {
                None | Some(GOAL_DRIVEN_GOAL_FILE) => Ok(target_dir.join(GOAL_DRIVEN_GOAL_FILE)),
                Some(name) => Err(anyhow!(
                    "workflow targets only expose {GOAL_DRIVEN_GOAL_FILE} for editing, got '{name}'"
                )),
            };
        }

        Ok(self.resolve_prompt(target, requested_file)?.path)
    }

    pub async fn run_target<D>(
        &self,
        target: &str,
        prompt_name: Option<&str>,
        delegate: &mut D,
    ) -> Result<TargetSummary>
    where
        D: RunDelegate,
    {
        self.run_target_with_control(target, prompt_name, RunControl::new(), delegate)
            .await
    }

    pub async fn run_prompt_file<D>(
        &self,
        prompt_path: &Utf8Path,
        delegate: &mut D,
    ) -> Result<LastRunStatus>
    where
        D: RunDelegate,
    {
        self.run_prompt_file_with_control(prompt_path, RunControl::new(), delegate)
            .await
    }

    pub async fn run_target_with_control<D>(
        &self,
        target: &str,
        prompt_name: Option<&str>,
        control: RunControl,
        delegate: &mut D,
    ) -> Result<TargetSummary>
    where
        D: RunDelegate,
    {
        let target_config = self.store.read_target_config(target)?;
        match target_config.mode {
            Some(WorkflowMode::GoalDriven) => {
                return self
                    .run_goal_driven_target_with_control(
                        target,
                        prompt_name,
                        target_config,
                        control,
                        delegate,
                    )
                    .await;
            }
            Some(WorkflowMode::TaskBased) => {
                return self
                    .run_task_based_target_with_control(
                        target,
                        prompt_name,
                        target_config,
                        control,
                        delegate,
                    )
                    .await;
            }
            None => {}
        }

        let target_summary = self.store.load_target(target)?;
        let prompt = self.select_prompt(&target_summary, prompt_name)?;
        let prepared = self.prepare_prompt_run(&prompt.path, &target_summary.dir)?;
        let max_iterations = self
            .store
            .read_target_config(target)?
            .max_iterations
            .unwrap_or(self.config.max_iterations);
        let status = self
            .run_prepared_prompt(
                &prepared,
                max_iterations,
                &control,
                delegate,
                &format!("Run complete for {}", target_summary.id),
                &format!("Reached max iterations for {}", target_summary.id),
            )
            .await
            .inspect_err(|_| {
                let status = if control.is_cancelled() {
                    LastRunStatus::Canceled
                } else {
                    LastRunStatus::Failed
                };
                let _ = self
                    .store
                    .set_last_run(target, &prepared.prompt_name, status);
            })?;

        self.store
            .set_last_run(target, &prepared.prompt_name, status)?;
        self.store.load_target(target)
    }

    pub async fn run_prompt_file_with_control<D>(
        &self,
        prompt_path: &Utf8Path,
        control: RunControl,
        delegate: &mut D,
    ) -> Result<LastRunStatus>
    where
        D: RunDelegate,
    {
        let target_dir = prompt_path.parent().ok_or_else(|| {
            anyhow!("prompt path '{prompt_path}' must have a parent directory for TARGET_DIR")
        })?;
        let prepared = self.prepare_prompt_run(prompt_path, target_dir)?;
        self.run_prepared_prompt(
            &prepared,
            self.config.max_iterations,
            &control,
            delegate,
            &format!("Run complete for {}", prompt_path),
            &format!("Reached max iterations for {}", prompt_path),
        )
        .await
    }

    async fn run_goal_driven_target_with_control<D>(
        &self,
        target: &str,
        prompt_name: Option<&str>,
        mut target_config: TargetConfig,
        control: RunControl,
        delegate: &mut D,
    ) -> Result<TargetSummary>
    where
        D: RunDelegate,
    {
        if let Some(prompt_name) = prompt_name {
            return Err(anyhow!(
                "goal_driven targets select plan/build internally; remove --prompt ('{prompt_name}')"
            ));
        }

        let target_dir = self.store.target_paths(target)?.dir;
        let hashes = self.goal_driven_hashes(&target_dir)?;
        let action = self.select_goal_driven_action(&target_config, &hashes);

        if action == GoalDrivenAction::Paused {
            target_config.last_prompt = Some(GOAL_DRIVEN_PAUSED_PROMPT.to_owned());
            target_config.last_run_status = LastRunStatus::Completed;
            self.store.write_target_config(&target_config)?;
            delegate
                .on_event(RunEvent::Note(format!(
                    "{target} is paused; edit {GOAL_DRIVEN_GOAL_FILE} to trigger re-planning."
                )))
                .await?;
            delegate
                .on_event(RunEvent::Finished {
                    status: LastRunStatus::Completed,
                    summary: format!("No run needed for {}", target),
                })
                .await?;
            return self.store.load_target(target);
        }

        let (prompt_name, prompt_text, completed_summary, max_iterations_summary, phase) =
            match action {
                GoalDrivenAction::Plan => (
                    GOAL_DRIVEN_PLAN_PROMPT.to_owned(),
                    goal_driven_plan_prompt(),
                    format!("Planning complete for {}", target),
                    format!("Reached max iterations while planning {}", target),
                    GoalDrivenPhase::Plan,
                ),
                GoalDrivenAction::Build => (
                    GOAL_DRIVEN_BUILD_PROMPT.to_owned(),
                    goal_driven_build_prompt(),
                    format!("Build complete for {}", target),
                    format!("Reached max iterations while building {}", target),
                    GoalDrivenPhase::Build,
                ),
                GoalDrivenAction::Paused => unreachable!(),
            };

        target_config.inflight = Some(GoalDrivenInflight {
            phase,
            goal_hash: hashes.goal_hash.clone(),
            content_hash: hashes.content_hash.clone(),
            started_at: current_unix_timestamp(),
        });
        self.store.write_target_config(&target_config)?;

        let prepared = self.prepare_inline_prompt_run(&target_dir, &prompt_name, &prompt_text)?;
        let max_iterations = target_config
            .max_iterations
            .unwrap_or(self.config.max_iterations);
        let status = self
            .run_prepared_prompt(
                &prepared,
                max_iterations,
                &control,
                delegate,
                &completed_summary,
                &max_iterations_summary,
            )
            .await
            .inspect_err(|_| {
                let status = if control.is_cancelled() {
                    LastRunStatus::Canceled
                } else {
                    LastRunStatus::Failed
                };
                let _ = self.update_goal_driven_last_run(target, &prompt_name, status);
            })?;

        if status == LastRunStatus::Completed {
            let after_hashes = self.goal_driven_hashes(&target_dir)?;
            let workflow = target_config
                .workflow
                .get_or_insert_with(GoalDrivenWorkflowState::default);
            workflow.last_goal_hash = Some(after_hashes.goal_hash);
            workflow.last_content_hash = Some(after_hashes.content_hash);
            match action {
                GoalDrivenAction::Plan => {
                    workflow.phase = GoalDrivenPhase::Build;
                    workflow.last_planned_at = Some(current_unix_timestamp());
                }
                GoalDrivenAction::Build => {
                    workflow.phase = GoalDrivenPhase::Paused;
                    workflow.last_built_at = Some(current_unix_timestamp());
                }
                GoalDrivenAction::Paused => {}
            }
            target_config.inflight = None;
        }

        target_config.last_prompt = Some(prompt_name.clone());
        target_config.last_run_status = status;
        self.store.write_target_config(&target_config)?;
        self.store.load_target(target)
    }

    async fn run_task_based_target_with_control<D>(
        &self,
        target: &str,
        prompt_name: Option<&str>,
        mut target_config: TargetConfig,
        control: RunControl,
        delegate: &mut D,
    ) -> Result<TargetSummary>
    where
        D: RunDelegate,
    {
        if let Some(prompt_name) = prompt_name {
            return Err(anyhow!(
                "task_based targets select the build loop internally; remove --prompt ('{prompt_name}')"
            ));
        }

        let target_dir = self.store.target_paths(target)?.dir;
        let hashes = self.task_based_hashes(&target_dir)?;
        let should_build = self.select_task_based_build_needed(&target_config, &hashes);

        if !should_build {
            target_config.last_prompt = Some(TASK_BASED_PAUSED_PROMPT.to_owned());
            target_config.last_run_status = LastRunStatus::Completed;
            self.store.write_target_config(&target_config)?;
            delegate
                .on_event(RunEvent::Note(format!(
                    "{target} is paused; edit {GOAL_DRIVEN_GOAL_FILE} or {TASK_BASED_PROGRESS_FILE} to resume work."
                )))
                .await?;
            delegate
                .on_event(RunEvent::Finished {
                    status: LastRunStatus::Completed,
                    summary: format!("No run needed for {}", target),
                })
                .await?;
            return self.store.load_target(target);
        }

        target_config.inflight = Some(GoalDrivenInflight {
            phase: GoalDrivenPhase::Build,
            goal_hash: hashes.goal_hash.clone(),
            content_hash: hashes.content_hash.clone(),
            started_at: current_unix_timestamp(),
        });
        self.store.write_target_config(&target_config)?;

        let prompt_name = TASK_BASED_BUILD_PROMPT.to_owned();
        let prepared =
            self.prepare_inline_prompt_run(&target_dir, &prompt_name, &task_based_build_prompt())?;
        let max_iterations = target_config
            .max_iterations
            .unwrap_or(self.config.max_iterations);
        let status = self
            .run_prepared_prompt(
                &prepared,
                max_iterations,
                &control,
                delegate,
                &format!("Build complete for {}", target),
                &format!("Reached max iterations while building {}", target),
            )
            .await
            .inspect_err(|_| {
                let status = if control.is_cancelled() {
                    LastRunStatus::Canceled
                } else {
                    LastRunStatus::Failed
                };
                let _ = self.update_goal_driven_last_run(target, &prompt_name, status);
            })?;

        if status == LastRunStatus::Completed {
            let after_hashes = self.task_based_hashes(&target_dir)?;
            let workflow = target_config
                .workflow
                .get_or_insert_with(GoalDrivenWorkflowState::default);
            workflow.phase = GoalDrivenPhase::Paused;
            workflow.last_goal_hash = Some(after_hashes.goal_hash);
            workflow.last_content_hash = Some(after_hashes.content_hash);
            workflow.last_built_at = Some(current_unix_timestamp());
            target_config.inflight = None;
        }

        target_config.last_prompt = Some(prompt_name);
        target_config.last_run_status = status;
        self.store.write_target_config(&target_config)?;
        self.store.load_target(target)
    }

    fn prepare_prompt_run(
        &self,
        prompt_path: &Utf8Path,
        target_dir: &Utf8Path,
    ) -> Result<PreparedPromptRun> {
        let prompt_name = prompt_path
            .file_name()
            .ok_or_else(|| anyhow!("prompt path '{prompt_path}' has no file name"))?
            .to_owned();
        let prepared = PreparedPromptRun {
            prompt_path: prompt_path.to_path_buf(),
            prompt_name,
            target_dir: target_dir.to_path_buf(),
            raw_prompt: None,
        };
        self.parse_prompt_run(&prepared)?;
        Ok(prepared)
    }

    fn prepare_inline_prompt_run(
        &self,
        target_dir: &Utf8Path,
        prompt_name: &str,
        prompt_text: &str,
    ) -> Result<PreparedPromptRun> {
        let prepared = PreparedPromptRun {
            prompt_path: target_dir.join(format!(".{prompt_name}.md")),
            prompt_name: prompt_name.to_owned(),
            target_dir: target_dir.to_path_buf(),
            raw_prompt: Some(prompt_text.to_owned()),
        };
        self.parse_prompt_run(&prepared)?;
        Ok(prepared)
    }

    fn parse_prompt_run(&self, prepared: &PreparedPromptRun) -> Result<ParsedPrompt> {
        let raw_prompt = match &prepared.raw_prompt {
            Some(raw_prompt) => raw_prompt.clone(),
            None => self
                .store
                .read_file(&prepared.prompt_path)
                .with_context(|| format!("failed to read prompt file {}", prepared.prompt_path))?,
        };
        let interpolated_prompt = interpolate_prompt_env(
            &raw_prompt,
            &self.project_dir,
            &prepared.target_dir,
            &prepared.prompt_path,
            &prepared.prompt_name,
        )?;
        Ok(parse_prompt_directives(&interpolated_prompt))
    }

    fn update_goal_driven_last_run(
        &self,
        target: &str,
        prompt_name: &str,
        status: LastRunStatus,
    ) -> Result<()> {
        let mut config = self.store.read_target_config(target)?;
        config.last_prompt = Some(prompt_name.to_owned());
        config.last_run_status = status;
        self.store.write_target_config(&config)
    }

    fn select_goal_driven_action(
        &self,
        config: &TargetConfig,
        hashes: &GoalDrivenHashes,
    ) -> GoalDrivenAction {
        if let Some(inflight) = &config.inflight {
            return match inflight.phase {
                GoalDrivenPhase::Plan => GoalDrivenAction::Plan,
                GoalDrivenPhase::Build => {
                    if inflight.goal_hash == hashes.goal_hash {
                        GoalDrivenAction::Build
                    } else {
                        GoalDrivenAction::Plan
                    }
                }
                GoalDrivenPhase::Paused => GoalDrivenAction::Paused,
            };
        }

        let workflow = config.workflow.clone().unwrap_or_default();
        if workflow
            .last_goal_hash
            .as_deref()
            .is_none_or(|hash| hash != hashes.goal_hash)
        {
            return GoalDrivenAction::Plan;
        }

        match workflow.phase {
            GoalDrivenPhase::Plan => GoalDrivenAction::Plan,
            GoalDrivenPhase::Build => GoalDrivenAction::Build,
            GoalDrivenPhase::Paused => {
                if workflow.last_content_hash.as_deref() == Some(hashes.content_hash.as_str()) {
                    GoalDrivenAction::Paused
                } else {
                    GoalDrivenAction::Plan
                }
            }
        }
    }

    fn select_task_based_build_needed(
        &self,
        config: &TargetConfig,
        hashes: &TaskBasedHashes,
    ) -> bool {
        if config.inflight.is_some() {
            return true;
        }

        let workflow = config.workflow.clone().unwrap_or_default();
        if workflow
            .last_goal_hash
            .as_deref()
            .is_none_or(|hash| hash != hashes.goal_hash)
        {
            return true;
        }

        match workflow.phase {
            GoalDrivenPhase::Paused => {
                workflow.last_content_hash.as_deref() != Some(hashes.content_hash.as_str())
            }
            GoalDrivenPhase::Plan | GoalDrivenPhase::Build => true,
        }
    }

    fn goal_driven_hashes(&self, target_dir: &Utf8Path) -> Result<GoalDrivenHashes> {
        let goal_path = target_dir.join(GOAL_DRIVEN_GOAL_FILE);
        let goal_contents = self
            .store
            .read_file(&goal_path)
            .with_context(|| format!("missing required goal file {}", goal_path))?;
        let goal_hash = hash_bytes(goal_contents.as_bytes());

        let mut hasher = Sha256::new();
        hash_named_contents(&mut hasher, GOAL_DRIVEN_GOAL_FILE, goal_contents.as_bytes());

        let plan_path = target_dir.join(GOAL_DRIVEN_PLAN_FILE);
        if plan_path.exists() {
            hash_named_contents(
                &mut hasher,
                GOAL_DRIVEN_PLAN_FILE,
                self.store.read_file(&plan_path)?.as_bytes(),
            );
        }

        let specs_dir = target_dir.join(GOAL_DRIVEN_SPECS_DIR);
        if specs_dir.exists() {
            let mut paths = walk_files(&specs_dir)?;
            paths.sort();
            for path in paths {
                let relative = path
                    .strip_prefix(target_dir)
                    .map_err(|_| anyhow!("failed to compute relative path for {}", path))?;
                hash_named_contents(
                    &mut hasher,
                    relative.as_str(),
                    self.store.read_file(&path)?.as_bytes(),
                );
            }
        }

        Ok(GoalDrivenHashes {
            goal_hash,
            content_hash: format!("sha256:{:x}", hasher.finalize()),
        })
    }

    fn task_based_hashes(&self, target_dir: &Utf8Path) -> Result<TaskBasedHashes> {
        let goal_path = target_dir.join(GOAL_DRIVEN_GOAL_FILE);
        let goal_contents = self
            .store
            .read_file(&goal_path)
            .with_context(|| format!("missing required goal file {}", goal_path))?;
        let goal_hash = hash_bytes(goal_contents.as_bytes());

        let mut hasher = Sha256::new();
        hash_named_contents(&mut hasher, GOAL_DRIVEN_GOAL_FILE, goal_contents.as_bytes());

        let progress_path = target_dir.join(TASK_BASED_PROGRESS_FILE);
        if progress_path.exists() {
            hash_named_contents(
                &mut hasher,
                TASK_BASED_PROGRESS_FILE,
                self.store.read_file(&progress_path)?.as_bytes(),
            );
        }

        Ok(TaskBasedHashes {
            goal_hash,
            content_hash: format!("sha256:{:x}", hasher.finalize()),
        })
    }

    async fn run_prepared_prompt<D>(
        &self,
        prepared: &PreparedPromptRun,
        max_iterations: usize,
        control: &RunControl,
        delegate: &mut D,
        completed_summary: &str,
        max_iterations_summary: &str,
    ) -> Result<LastRunStatus>
    where
        D: RunDelegate,
    {
        if max_iterations == 0 {
            return Err(anyhow!("max_iterations must be greater than zero"));
        }

        for iteration in 1..=max_iterations {
            let parsed_prompt = self.parse_prompt_run(prepared)?;
            let criteria_before =
                self.read_completion_inputs(&parsed_prompt.completion_criteria)?;

            if control.is_cancelled() {
                return Err(anyhow!("operation canceled"));
            }

            delegate
                .on_event(RunEvent::IterationStarted {
                    prompt_name: prepared.prompt_name.clone(),
                    iteration,
                    max_iterations,
                })
                .await?;

            let config = self.runner_config_for(control);
            let result = self
                .execute_runner(
                    &config,
                    RunnerInvocation {
                        prompt_text: parsed_prompt.prompt_text.clone(),
                        project_dir: self.project_dir.clone(),
                        target_dir: prepared.target_dir.clone(),
                        prompt_path: prepared.prompt_path.clone(),
                        prompt_name: prepared.prompt_name.clone(),
                    },
                    control,
                    delegate,
                )
                .await?;

            if result.exit_code != 0 {
                let message = format!("runner exited with code {}", result.exit_code);
                delegate.on_event(RunEvent::Note(message.clone())).await?;
                return Err(anyhow!(message));
            }

            if !parsed_prompt.completion_criteria.is_empty()
                && self.completion_criteria_satisfied(
                    &parsed_prompt.completion_criteria,
                    &criteria_before,
                    &self.read_completion_inputs(&parsed_prompt.completion_criteria)?,
                )?
            {
                delegate
                    .on_event(RunEvent::Finished {
                        status: LastRunStatus::Completed,
                        summary: completed_summary.to_owned(),
                    })
                    .await?;
                return Ok(LastRunStatus::Completed);
            }
        }

        delegate
            .on_event(RunEvent::Finished {
                status: LastRunStatus::MaxIterations,
                summary: max_iterations_summary.to_owned(),
            })
            .await?;
        Ok(LastRunStatus::MaxIterations)
    }

    fn read_completion_inputs(
        &self,
        criteria: &[CompletionCriterion],
    ) -> Result<Vec<Option<String>>> {
        criteria
            .iter()
            .map(|criterion| {
                let name = match criterion {
                    CompletionCriterion::Watch { path }
                    | CompletionCriterion::NoLineContainsAll { path, .. } => path,
                };
                let watch_path = Utf8Path::new(name);
                let path = if watch_path.is_absolute() {
                    watch_path.to_path_buf()
                } else {
                    self.project_dir.join(watch_path)
                };
                match std::fs::read_to_string(&path) {
                    Ok(contents) => Ok(Some(contents)),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
                    Err(error) => {
                        Err(error).with_context(|| format!("failed to read watched file {}", path))
                    }
                }
            })
            .collect()
    }

    fn completion_criteria_satisfied(
        &self,
        criteria: &[CompletionCriterion],
        before: &[Option<String>],
        after: &[Option<String>],
    ) -> Result<bool> {
        if before.len() != criteria.len() || after.len() != criteria.len() {
            return Err(anyhow!("completion criteria state length mismatch"));
        }

        Ok(criteria.iter().zip(before.iter().zip(after.iter())).all(
            |(criterion, (before, after))| match criterion {
                CompletionCriterion::Watch { .. } => before == after,
                CompletionCriterion::NoLineContainsAll { tokens, .. } => {
                    after.as_deref().is_some_and(|contents| {
                        !contents.lines().any(|line| {
                            tokens
                                .iter()
                                .all(|token| !token.is_empty() && line.contains(token))
                        })
                    })
                }
            },
        ))
    }

    pub fn resolve_prompt(
        &self,
        target: &str,
        prompt_name: Option<&str>,
    ) -> Result<ralph_core::PromptFile> {
        let target_summary = self.store.load_target(target)?;
        self.select_prompt(&target_summary, prompt_name)
    }

    fn select_prompt(
        &self,
        target_summary: &TargetSummary,
        prompt_name: Option<&str>,
    ) -> Result<ralph_core::PromptFile> {
        if matches!(
            target_summary.mode,
            Some(WorkflowMode::GoalDriven | WorkflowMode::TaskBased)
        ) {
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

    fn runner_config_for(&self, control: &RunControl) -> RunnerConfig {
        let detected = CodingAgent::detected();
        if let Some(agent) = control.coding_agent() {
            return RunnerConfig::for_agent(resolve_available_agent(agent, &detected));
        }

        if let Some(agent) = self.config.runner.inferred_agent() {
            let resolved = resolve_available_agent(agent, &detected);
            if resolved != agent {
                return RunnerConfig::for_agent(resolved);
            }
        }

        self.config.runner.clone()
    }

    async fn execute_runner<D>(
        &self,
        config: &RunnerConfig,
        invocation: RunnerInvocation,
        control: &RunControl,
        delegate: &mut D,
    ) -> Result<ralph_core::RunnerResult>
    where
        D: RunDelegate,
    {
        let (stream_tx, mut stream_rx) = tokio::sync::mpsc::unbounded_channel();
        let run = self
            .runner
            .run(config, invocation, control, Some(stream_tx));
        tokio::pin!(run);

        loop {
            tokio::select! {
                result = &mut run => {
                    while let Some(event) = stream_rx.recv().await {
                        forward_stream_event(delegate, event).await?;
                    }
                    return result;
                }
                maybe = stream_rx.recv() => {
                    if let Some(event) = maybe {
                        forward_stream_event(delegate, event).await?;
                    }
                }
            }
        }
    }
}

async fn forward_stream_event<D>(delegate: &mut D, event: RunnerStreamEvent) -> Result<()>
where
    D: RunDelegate,
{
    match event {
        RunnerStreamEvent::Output(chunk) => delegate.on_event(RunEvent::Output(chunk)).await,
    }
}

fn parse_prompt_directives(prompt_text: &str) -> ParsedPrompt {
    let mut completion_criteria = Vec::new();
    let mut cleaned_lines = Vec::new();

    for line in prompt_text.lines() {
        let trimmed = line.trim();
        let directive = serde_json::from_str::<PromptDirective>(trimmed);
        match directive {
            Ok(PromptDirective::Watch { path }) if !path.trim().is_empty() => {
                completion_criteria.push(CompletionCriterion::Watch { path });
            }
            Ok(PromptDirective::CompleteWhen { kind, path, tokens })
                if !path.trim().is_empty() && !tokens.is_empty() =>
            {
                match kind {
                    CompletionDirectiveType::NoLineContainsAll => {
                        completion_criteria
                            .push(CompletionCriterion::NoLineContainsAll { path, tokens });
                    }
                }
            }
            _ => {
                if !line.trim().is_empty() {
                    cleaned_lines.push(line.to_owned());
                }
            }
        }
    }

    ParsedPrompt {
        prompt_text: cleaned_lines.join("\n"),
        completion_criteria,
    }
}

fn interpolate_prompt_env(
    prompt_text: &str,
    project_dir: &Utf8Path,
    target_dir: &Utf8Path,
    prompt_path: &Utf8Path,
    prompt_name: &str,
) -> Result<String> {
    let replacements = [
        (RALPH_ENV_PROJECT_DIR, absolute_unix_path(project_dir)?),
        (RALPH_ENV_TARGET_DIR, absolute_unix_path(target_dir)?),
        (RALPH_ENV_PROMPT_PATH, absolute_unix_path(prompt_path)?),
        (RALPH_ENV_PROMPT_NAME, prompt_name.to_owned()),
    ];

    let mut interpolated = prompt_text.to_owned();
    for (needle, value) in replacements {
        interpolated = interpolated.replace(needle, &value);
    }
    Ok(interpolated)
}

fn absolute_unix_path(path: &Utf8Path) -> Result<String> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        let cwd =
            Utf8PathBuf::from_path_buf(std::env::current_dir().context("failed to read cwd")?)
                .map_err(|_| anyhow!("current directory is not valid UTF-8"))?;
        cwd.join(path)
    };
    Ok(absolute.as_str().replace('\\', "/"))
}

fn resolve_available_agent(preferred: CodingAgent, detected: &[CodingAgent]) -> CodingAgent {
    if detected.is_empty() || detected.contains(&preferred) {
        preferred
    } else {
        detected[0]
    }
}

fn goal_driven_plan_prompt() -> String {
    format!(
        r#"1. Study these inputs before planning:
   a. Study `{target_dir}/GOAL.md`.
   b. Study `{target_dir}/plan.toml` if it exists.
   c. Study all spec files in `{target_dir}/specs/`.
2. Study the relevant repository documentation and source code (do not assume something is not implemented, look deeply).
3. Create or revise the spec files in `{target_dir}/specs/` until the intended behavior, constraints, and user-visible outcomes are clear enough to build against without guessing.
4. Only after the specifications are coherent and sufficient, create or revise `{target_dir}/plan.toml` as the current operational plan.
5. `plan.toml` must stay valid TOML and follow this exact shape:

```toml
version = 1

[[items]]
category = "functional"
description = "Describe one concrete outcome"
steps = ["List the ordered implementation and verification steps"]
completed = false
```

6. Keep the items in the exact execution order. Earlier items must prepare later items; do not rely on later items to make earlier items possible.
7. Keep every incomplete item at `completed = false`.
8. Plan only. Do not implement product code or tests.
9. If the specifications and `plan.toml` are already correct and sufficient, leave `{target_dir}/plan.toml` unchanged.

{{"ralph":"watch","path":"{target_dir}/plan.toml"}}
"#,
        target_dir = RALPH_ENV_TARGET_DIR
    )
}

fn goal_driven_build_prompt() -> String {
    format!(
        r#"1. Study these inputs before building:
   a. Study `{target_dir}/GOAL.md`.
   b. Study `{target_dir}/plan.toml`.
   c. Study all spec files in `{target_dir}/specs/`.
   d. Study `{target_dir}/journal.txt` if it exists.
   e. Study `AGENTS.md` if it exists.
2. Study the relevant repository documentation and source code (do not assume something is not implemented, look deeply).
3. Select the single highest-priority open item with the highest leverage from `{target_dir}/plan.toml`.
4. Execute only that item completely against the current target-local specifications and plan. Do not leave placeholders or partial implementations behind.
5. Run the checks relevant to the code you changed.
6. Update `{target_dir}/plan.toml` so it accurately records completed work and any remaining follow-up.
7. Create or update `{target_dir}/journal.txt` as a free-form builder journal for future iterations. Record what you changed, what you verified, and any concrete follow-up notes useful to the next build iteration.
8. Do not edit `{target_dir}/GOAL.md`.
9. Update `AGENTS.md` only when you learn durable operational guidance about running or debugging the project.

{{"ralph":"complete_when","type":"no_line_contains_all","path":"{target_dir}/plan.toml","tokens":["completed","false"]}}
"#,
        target_dir = RALPH_ENV_TARGET_DIR
    )
}

fn task_based_build_prompt() -> String {
    format!(
        r#"1. Study these inputs before building:
   a. Study `{target_dir}/GOAL.md` as the CEO input.
   b. Study `{target_dir}/progress.toml`.
   c. Study `{target_dir}/{journal_file}` if it exists.
   d. Study `AGENTS.md` if it exists.
2. Study the relevant repository documentation and source code (do not assume something is not implemented, look deeply).
3. Select the single highest-priority open item with the highest leverage from `{target_dir}/progress.toml`.
4. Execute only that item completely. Do not leave placeholders or partial implementations behind.
5. Run the checks relevant to the code you changed.
6. Update `{target_dir}/progress.toml` so it accurately records completed work and any remaining follow-up.
7. Create or update `{target_dir}/{journal_file}` as a free-form builder journal for future iterations. Record what you changed, what you verified, and any concrete follow-up notes useful to the next build iteration.
8. Do not edit `{target_dir}/GOAL.md`.
9. Update `AGENTS.md` only when you learn durable operational guidance about running or debugging the project.

{{"ralph":"complete_when","type":"no_line_contains_all","path":"{target_dir}/progress.toml","tokens":["completed","false"]}}
"#,
        target_dir = RALPH_ENV_TARGET_DIR,
        journal_file = TASK_BASED_JOURNAL_FILE
    )
}

fn current_unix_timestamp() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

fn hash_bytes(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("sha256:{:x}", hasher.finalize())
}

fn hash_named_contents(hasher: &mut Sha256, name: &str, contents: &[u8]) {
    hasher.update(name.as_bytes());
    hasher.update([0]);
    hasher.update(contents);
    hasher.update([0xff]);
}

fn walk_files(dir: &Utf8Path) -> Result<Vec<Utf8PathBuf>> {
    let mut files = Vec::new();
    for entry in fs::read_dir(dir).with_context(|| format!("failed to read {}", dir))? {
        let entry = entry?;
        let path = Utf8PathBuf::from_path_buf(entry.path())
            .map_err(|_| anyhow!("non-UTF8 path under {}", dir))?;
        if path.is_dir() {
            files.extend(walk_files(&path)?);
        } else if path.is_file() {
            files.push(path);
        }
    }
    Ok(files)
}

#[derive(Default)]
pub struct ConsoleDelegate;

#[async_trait]
impl RunDelegate for ConsoleDelegate {
    async fn on_event(&mut self, event: RunEvent) -> Result<()> {
        match event {
            RunEvent::IterationStarted {
                prompt_name,
                iteration,
                max_iterations,
            } => {
                println!(
                    "{}",
                    format_iteration_banner(&prompt_name, iteration, max_iterations)
                );
            }
            RunEvent::Output(chunk) => {
                print!("{chunk}");
            }
            RunEvent::Note(note) => {
                eprintln!("{note}");
            }
            RunEvent::Finished { status, summary } => {
                println!("\n{} ({})", summary, status.label());
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use anyhow::Result;
    use async_trait::async_trait;
    use camino::Utf8PathBuf;
    use ralph_core::{
        AppConfig, CodingAgent, GoalDrivenPhase, RunControl, RunnerInvocation, RunnerResult,
        ScaffoldId,
    };
    use ralph_runner::{RunnerAdapter, RunnerStreamEvent};
    use tokio::sync::mpsc::UnboundedSender;

    use crate::{
        CompletionCriterion, RalphApp, RunDelegate, RunEvent, interpolate_prompt_env,
        parse_prompt_directives, resolve_available_agent,
    };

    #[derive(Clone)]
    struct ScriptedRunner {
        output: String,
        exit_code: i32,
    }

    #[derive(Clone)]
    struct SteeringRunner {
        seen_prompts: Arc<Mutex<Vec<String>>>,
    }

    #[derive(Clone)]
    struct GoalDrivenRunner {
        seen_prompt_names: Arc<Mutex<Vec<String>>>,
    }

    #[derive(Clone)]
    struct TaskBasedRunner {
        seen_prompt_names: Arc<Mutex<Vec<String>>>,
    }

    #[async_trait]
    impl RunnerAdapter for ScriptedRunner {
        async fn run(
            &self,
            _config: &ralph_core::RunnerConfig,
            _invocation: RunnerInvocation,
            _control: &RunControl,
            stream: Option<UnboundedSender<RunnerStreamEvent>>,
        ) -> Result<RunnerResult> {
            if let Some(stream) = stream {
                let _ = stream.send(RunnerStreamEvent::Output(self.output.clone()));
            }
            Ok(RunnerResult {
                output: self.output.clone(),
                exit_code: self.exit_code,
            })
        }
    }

    #[async_trait]
    impl RunnerAdapter for SteeringRunner {
        async fn run(
            &self,
            _config: &ralph_core::RunnerConfig,
            invocation: RunnerInvocation,
            _control: &RunControl,
            _stream: Option<UnboundedSender<RunnerStreamEvent>>,
        ) -> Result<RunnerResult> {
            let iteration = {
                let mut seen = self.seen_prompts.lock().unwrap();
                seen.push(invocation.prompt_text.clone());
                seen.len()
            };

            if iteration == 1 {
                std::fs::write(&invocation.prompt_path, "# Prompt\n\nSecond version\n").unwrap();
            }

            Ok(RunnerResult {
                output: String::new(),
                exit_code: 0,
            })
        }
    }

    #[async_trait]
    impl RunnerAdapter for GoalDrivenRunner {
        async fn run(
            &self,
            _config: &ralph_core::RunnerConfig,
            invocation: RunnerInvocation,
            _control: &RunControl,
            _stream: Option<UnboundedSender<RunnerStreamEvent>>,
        ) -> Result<RunnerResult> {
            self.seen_prompt_names
                .lock()
                .unwrap()
                .push(invocation.prompt_name.clone());

            let plan_path = invocation.target_dir.join("plan.toml");
            let contents = match invocation.prompt_name.as_str() {
                crate::GOAL_DRIVEN_PLAN_PROMPT => {
                    "version = 1\n\n[[items]]\ncategory = \"functional\"\ndescription = \"Ship the feature\"\nsteps = [\"Implement it\", \"Verify it\"]\ncompleted = false\n".to_owned()
                }
                crate::GOAL_DRIVEN_BUILD_PROMPT => {
                    "version = 1\n\n[[items]]\ncategory = \"functional\"\ndescription = \"Ship the feature\"\nsteps = [\"Implement it\", \"Verify it\"]\ncompleted = true\n".to_owned()
                }
                other => panic!("unexpected goal-driven prompt {other}"),
            };
            std::fs::write(plan_path, contents).unwrap();

            Ok(RunnerResult {
                output: String::new(),
                exit_code: 0,
            })
        }
    }

    #[async_trait]
    impl RunnerAdapter for TaskBasedRunner {
        async fn run(
            &self,
            _config: &ralph_core::RunnerConfig,
            invocation: RunnerInvocation,
            _control: &RunControl,
            _stream: Option<UnboundedSender<RunnerStreamEvent>>,
        ) -> Result<RunnerResult> {
            self.seen_prompt_names
                .lock()
                .unwrap()
                .push(invocation.prompt_name.clone());

            let progress_path = invocation.target_dir.join("progress.toml");
            let contents = match invocation.prompt_name.as_str() {
                crate::TASK_BASED_BUILD_PROMPT => {
                    "version = 1\n\n[[items]]\ndescription = \"Ship the feature\"\nsteps = [\"Implement it\", \"Verify it\"]\ncompleted = true\n".to_owned()
                }
                other => panic!("unexpected task-based prompt {other}"),
            };
            std::fs::write(progress_path, contents).unwrap();

            Ok(RunnerResult {
                output: String::new(),
                exit_code: 0,
            })
        }
    }

    #[derive(Default)]
    struct TestDelegate;

    #[async_trait]
    impl RunDelegate for TestDelegate {
        async fn on_event(&mut self, _event: RunEvent) -> Result<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn watched_prompts_complete_when_watched_files_are_unchanged() {
        let temp = tempfile::tempdir().unwrap();
        let project_dir = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();
        std::fs::write(project_dir.join("IMPLEMENTATION_PLAN.md"), "- item\n").unwrap();
        let app = RalphApp::new(
            project_dir.clone(),
            AppConfig::default(),
            ScriptedRunner {
                output: "no plan changes".to_owned(),
                exit_code: 0,
            },
        );
        app.create_target("demo", Some(ScaffoldId::SinglePrompt))
            .unwrap();
        std::fs::write(
            project_dir.join(".ralph/targets/demo/prompt_main.md"),
            "{\"ralph\":\"watch\",\"path\":\"IMPLEMENTATION_PLAN.md\"}\n\n# Prompt\n",
        )
        .unwrap();

        let mut delegate = TestDelegate;
        let summary = app.run_target("demo", None, &mut delegate).await.unwrap();

        assert_eq!(
            summary.last_run_status,
            ralph_core::LastRunStatus::Completed
        );
    }

    #[tokio::test]
    async fn single_prompt_targets_still_run_to_max_iterations_without_plan_change_stop() {
        let temp = tempfile::tempdir().unwrap();
        let project_dir = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();
        let mut config = AppConfig::default();
        config.max_iterations = 1;
        let app = RalphApp::new(
            project_dir.clone(),
            config,
            ScriptedRunner {
                output: "no stop protocol".to_owned(),
                exit_code: 0,
            },
        );
        app.create_target("demo", Some(ScaffoldId::SinglePrompt))
            .unwrap();
        std::fs::write(
            project_dir.join(".ralph/targets/demo/prompt_main.md"),
            "# Prompt\n\nNo watch directives here.\n",
        )
        .unwrap();

        let mut delegate = TestDelegate;
        let summary = app.run_target("demo", None, &mut delegate).await.unwrap();

        assert_eq!(
            summary.last_run_status,
            ralph_core::LastRunStatus::MaxIterations
        );
    }

    #[tokio::test]
    async fn prompt_edits_are_reloaded_between_iterations() {
        let temp = tempfile::tempdir().unwrap();
        let project_dir = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();
        let seen_prompts = Arc::new(Mutex::new(Vec::new()));
        let mut config = AppConfig::default();
        config.max_iterations = 2;
        let app = RalphApp::new(
            project_dir.clone(),
            config,
            SteeringRunner {
                seen_prompts: seen_prompts.clone(),
            },
        );
        app.create_target("demo", Some(ScaffoldId::SinglePrompt))
            .unwrap();
        std::fs::write(
            project_dir.join(".ralph/targets/demo/prompt_main.md"),
            "# Prompt\n\nFirst version\n",
        )
        .unwrap();

        let mut delegate = TestDelegate;
        let summary = app.run_target("demo", None, &mut delegate).await.unwrap();

        assert_eq!(
            summary.last_run_status,
            ralph_core::LastRunStatus::MaxIterations
        );
        assert_eq!(
            *seen_prompts.lock().unwrap(),
            vec![
                "# Prompt\nFirst version".to_owned(),
                "# Prompt\nSecond version".to_owned()
            ]
        );
    }

    #[test]
    fn prompt_directives_are_trimmed_before_runner_input() {
        let parsed = parse_prompt_directives(
            "{\"ralph\":\"watch\",\"path\":\"IMPLEMENTATION_PLAN.md\"}\n# Prompt\nBody\n{\"ralph\":\"complete_when\",\"type\":\"no_line_contains_all\",\"path\":\"specs/api.md\",\"tokens\":[\"completed\",\"false\"]}",
        );

        assert_eq!(
            parsed.completion_criteria,
            vec![
                CompletionCriterion::Watch {
                    path: "IMPLEMENTATION_PLAN.md".to_owned()
                },
                CompletionCriterion::NoLineContainsAll {
                    path: "specs/api.md".to_owned(),
                    tokens: vec!["completed".to_owned(), "false".to_owned()]
                }
            ]
        );
        assert_eq!(parsed.prompt_text, "# Prompt\nBody");
    }

    #[test]
    fn invalid_json_lines_are_not_treated_as_directives() {
        let parsed =
            parse_prompt_directives("{\"ralph\":\"watch\",\"path\":\"foo\"\n# Prompt\nBody");

        assert_eq!(
            parsed.prompt_text,
            "{\"ralph\":\"watch\",\"path\":\"foo\"\n# Prompt\nBody"
        );
        assert!(parsed.completion_criteria.is_empty());
    }

    #[test]
    fn ralph_env_target_dir_is_interpolated_to_absolute_unix_path() {
        let project_dir = Utf8PathBuf::from("/tmp/project");
        let target_dir = Utf8PathBuf::from("/tmp/project/.ralph/targets/demo");
        let prompt_path = target_dir.join("prompt_main.md");
        let interpolated = interpolate_prompt_env(
            "{\"ralph\":\"watch\",\"path\":\"{ralph-env:TARGET_DIR}/progress.txt\"}\nRead {ralph-env:TARGET_DIR}/progress.txt",
            &project_dir,
            &target_dir,
            &prompt_path,
            "prompt_main.md",
        )
        .unwrap();

        assert_eq!(
            interpolated,
            "{\"ralph\":\"watch\",\"path\":\"/tmp/project/.ralph/targets/demo/progress.txt\"}\nRead /tmp/project/.ralph/targets/demo/progress.txt"
        );
    }

    #[test]
    fn agent_resolution_falls_back_to_first_detected_agent() {
        assert_eq!(
            resolve_available_agent(
                CodingAgent::Codex,
                &[CodingAgent::Opencode, CodingAgent::Raijin]
            ),
            CodingAgent::Opencode
        );
        assert_eq!(
            resolve_available_agent(
                CodingAgent::Codex,
                &[CodingAgent::Codex, CodingAgent::Opencode]
            ),
            CodingAgent::Codex
        );
        assert_eq!(
            resolve_available_agent(CodingAgent::Raijin, &[]),
            CodingAgent::Raijin
        );
    }

    #[tokio::test]
    async fn goal_driven_targets_plan_then_build_then_pause() {
        let temp = tempfile::tempdir().unwrap();
        let project_dir = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();
        let seen_prompt_names = Arc::new(Mutex::new(Vec::new()));
        let app = RalphApp::new(
            project_dir.clone(),
            AppConfig::default(),
            GoalDrivenRunner {
                seen_prompt_names: seen_prompt_names.clone(),
            },
        );
        app.create_target("demo", Some(ScaffoldId::GoalDriven))
            .unwrap();

        let mut delegate = TestDelegate;
        let summary = app.run_target("demo", None, &mut delegate).await.unwrap();
        assert_eq!(
            summary.last_prompt.as_deref(),
            Some(crate::GOAL_DRIVEN_PLAN_PROMPT)
        );
        assert_eq!(
            summary.last_run_status,
            ralph_core::LastRunStatus::Completed
        );
        assert_eq!(
            app.store
                .read_target_config("demo")
                .unwrap()
                .workflow
                .as_ref()
                .map(|workflow| workflow.phase),
            Some(GoalDrivenPhase::Build)
        );

        let summary = app.run_target("demo", None, &mut delegate).await.unwrap();
        assert_eq!(
            summary.last_prompt.as_deref(),
            Some(crate::GOAL_DRIVEN_BUILD_PROMPT)
        );
        assert_eq!(
            app.store
                .read_target_config("demo")
                .unwrap()
                .workflow
                .as_ref()
                .map(|workflow| workflow.phase),
            Some(GoalDrivenPhase::Paused)
        );

        let summary = app.run_target("demo", None, &mut delegate).await.unwrap();
        assert_eq!(
            summary.last_prompt.as_deref(),
            Some(crate::GOAL_DRIVEN_PAUSED_PROMPT)
        );
        assert_eq!(
            *seen_prompt_names.lock().unwrap(),
            vec![
                crate::GOAL_DRIVEN_PLAN_PROMPT.to_owned(),
                crate::GOAL_DRIVEN_PLAN_PROMPT.to_owned(),
                crate::GOAL_DRIVEN_BUILD_PROMPT.to_owned()
            ]
        );

        std::fs::write(
            project_dir.join(".ralph/targets/demo/GOAL.md"),
            "# Goal\n\nChanged\n",
        )
        .unwrap();
        let summary = app.run_target("demo", None, &mut delegate).await.unwrap();
        assert_eq!(
            summary.last_prompt.as_deref(),
            Some(crate::GOAL_DRIVEN_PLAN_PROMPT)
        );
        assert_eq!(
            *seen_prompt_names.lock().unwrap(),
            vec![
                crate::GOAL_DRIVEN_PLAN_PROMPT.to_owned(),
                crate::GOAL_DRIVEN_PLAN_PROMPT.to_owned(),
                crate::GOAL_DRIVEN_BUILD_PROMPT.to_owned(),
                crate::GOAL_DRIVEN_PLAN_PROMPT.to_owned(),
                crate::GOAL_DRIVEN_PLAN_PROMPT.to_owned()
            ]
        );
    }

    #[tokio::test]
    async fn task_based_targets_build_then_pause_and_resume_on_goal_change() {
        let temp = tempfile::tempdir().unwrap();
        let project_dir = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();
        let seen_prompt_names = Arc::new(Mutex::new(Vec::new()));
        let app = RalphApp::new(
            project_dir.clone(),
            AppConfig::default(),
            TaskBasedRunner {
                seen_prompt_names: seen_prompt_names.clone(),
            },
        );
        app.create_target("demo", Some(ScaffoldId::TaskBased))
            .unwrap();

        let mut delegate = TestDelegate;
        let summary = app.run_target("demo", None, &mut delegate).await.unwrap();
        assert_eq!(
            summary.last_prompt.as_deref(),
            Some(crate::TASK_BASED_BUILD_PROMPT)
        );
        assert_eq!(
            summary.last_run_status,
            ralph_core::LastRunStatus::Completed
        );
        assert_eq!(
            app.store
                .read_target_config("demo")
                .unwrap()
                .workflow
                .as_ref()
                .map(|workflow| workflow.phase),
            Some(GoalDrivenPhase::Paused)
        );

        let summary = app.run_target("demo", None, &mut delegate).await.unwrap();
        assert_eq!(
            summary.last_prompt.as_deref(),
            Some(crate::TASK_BASED_PAUSED_PROMPT)
        );
        assert_eq!(
            *seen_prompt_names.lock().unwrap(),
            vec![crate::TASK_BASED_BUILD_PROMPT.to_owned()]
        );

        std::fs::write(
            project_dir.join(".ralph/targets/demo/GOAL.md"),
            "# Goal\n\nChanged\n",
        )
        .unwrap();
        let summary = app.run_target("demo", None, &mut delegate).await.unwrap();
        assert_eq!(
            summary.last_prompt.as_deref(),
            Some(crate::TASK_BASED_BUILD_PROMPT)
        );
        assert_eq!(
            *seen_prompt_names.lock().unwrap(),
            vec![
                crate::TASK_BASED_BUILD_PROMPT.to_owned(),
                crate::TASK_BASED_BUILD_PROMPT.to_owned()
            ]
        );
    }
}
