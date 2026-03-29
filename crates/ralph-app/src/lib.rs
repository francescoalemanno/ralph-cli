use std::{env, fs, process::Command};

use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use camino::{Utf8Path, Utf8PathBuf};
use ralph_core::{
    AppConfig, ArtifactStore, BuildPromptContext, BuilderMarker, ClarificationAnswer,
    ClarificationRequest, CodingAgent, ProgressRevisionPromptContext, ReviewData, RunControl,
    RunnerConfig, RunnerInvocation, RunnerMode, SpecPaths, SpecSummary, build_prompt,
    parse_builder_marker_from_output, parse_clarification_request,
    parse_planning_marker_from_output, planning_prompt, progress_revision_prompt,
    strip_persisted_promise_markers,
};
use ralph_runner::{CommandRunner, RunnerAdapter, RunnerStreamEvent};
use similar::TextDiff;
use tracing::debug;

#[derive(Debug, Clone)]
pub enum RunEvent {
    ArtifactsCreated {
        spec_path: String,
        progress_path: String,
        feedback_path: String,
    },
    IterationStarted {
        mode: RunnerMode,
        iteration: usize,
        max_iterations: usize,
    },
    Output(String),
    Note(String),
    Finished {
        mode: RunnerMode,
        completed: bool,
        summary: String,
    },
}

#[derive(Debug, Clone)]
pub struct SpecEditSession {
    pub target: String,
    pub paths: SpecPaths,
    pub past_spec_path: Utf8PathBuf,
}

#[derive(Debug, Clone)]
pub struct ProgressRevisionRequest {
    pub target: String,
    pub paths: SpecPaths,
    pub past_spec_path: Utf8PathBuf,
    pub diff_path: Utf8PathBuf,
}

pub fn format_iteration_banner(
    mode: RunnerMode,
    iteration: usize,
    max_iterations: usize,
) -> String {
    let title = format!(
        " {} ITERATION {}/{} ",
        mode.as_str().to_ascii_uppercase(),
        iteration,
        max_iterations
    );
    let width = title.len().max(44);
    let rule = "=".repeat(width);
    format!("\n{rule}\n{title:=^width$}\n{rule}", width = width)
}

#[async_trait]
pub trait RunDelegate: Send {
    async fn on_event(&mut self, event: RunEvent) -> Result<()>;

    async fn ask_clarification(
        &mut self,
        request: ClarificationRequest,
    ) -> Result<Option<ClarificationAnswer>>;
}

#[derive(Debug, Clone)]
pub struct RalphApp<R = CommandRunner> {
    project_dir: Utf8PathBuf,
    store: ArtifactStore,
    config: AppConfig,
    runner: R,
}

impl RalphApp<CommandRunner> {
    pub fn load(project_dir: impl Into<Utf8PathBuf>) -> Result<Self> {
        let project_dir = project_dir.into();
        let config = AppConfig::load(&project_dir)?;
        Ok(Self {
            store: ArtifactStore::new(project_dir.clone()),
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
            store: ArtifactStore::new(project_dir.clone()),
            project_dir,
            config,
            runner,
        }
    }

    pub fn config(&self) -> &AppConfig {
        &self.config
    }

    pub fn coding_agent(&self) -> CodingAgent {
        self.config.coding_agent()
    }

    pub fn set_coding_agent(&mut self, agent: CodingAgent) {
        self.config.set_coding_agent(agent);
    }

    pub fn project_dir(&self) -> &Utf8Path {
        &self.project_dir
    }

    pub fn list_specs(&self) -> Result<Vec<SpecSummary>> {
        self.store.list_specs()
    }

    pub fn review_target(&self, target: &str) -> Result<ReviewData> {
        let paths = self.store.resolve_target(target)?;
        self.store.review(&paths)
    }

    pub fn resolve_target(&self, target: &str) -> Result<SpecPaths> {
        self.store.resolve_target(target)
    }

    pub fn edit_target(&self, target: &str) -> Result<()> {
        let session = self.begin_spec_edit(target)?;
        self.open_in_editor(&session.paths.spec_path)
    }

    pub fn begin_spec_edit(&self, target: &str) -> Result<SpecEditSession> {
        let paths = self.store.resolve_target(target)?;
        self.store.ensure_ralph_dir()?;
        if !paths.spec_path.exists() {
            self.store.write_spec(&paths.spec_path, "")?;
        }
        let current_spec = self.store.read_spec(&paths.spec_path)?;
        let past_spec_path = self.store.past_spec_path(&paths.spec_path)?;
        self.store.write_auxiliary(&past_spec_path, &current_spec)?;

        Ok(SpecEditSession {
            target: target.to_owned(),
            paths,
            past_spec_path,
        })
    }

    pub fn finish_spec_edit(
        &self,
        session: SpecEditSession,
    ) -> Result<Option<ProgressRevisionRequest>> {
        let previous_spec = self.store.read_spec(&session.past_spec_path)?;
        let current_spec = self.store.read_spec(&session.paths.spec_path)?;
        if previous_spec == current_spec {
            self.remove_optional_file(&session.past_spec_path)?;
            return Ok(None);
        }

        let diff_path = self.store.spec_edit_diff_path(&session.paths.spec_path)?;
        let diff = render_spec_diff(
            &session.past_spec_path,
            &session.paths.spec_path,
            &previous_spec,
            &current_spec,
        );
        self.store.write_auxiliary(&diff_path, &diff)?;

        Ok(Some(ProgressRevisionRequest {
            target: session.target,
            paths: session.paths,
            past_spec_path: session.past_spec_path,
            diff_path,
        }))
    }

    pub fn edit_spec_session(&self, session: &SpecEditSession) -> Result<()> {
        self.open_in_editor(&session.paths.spec_path)
    }

    fn open_in_editor(&self, path: &Utf8Path) -> Result<()> {
        let editor = self
            .config
            .editor_override
            .clone()
            .or_else(|| env::var("VISUAL").ok())
            .or_else(|| env::var("EDITOR").ok())
            .unwrap_or_else(|| "vi".to_owned());

        let status = Command::new(&editor)
            .arg(path.as_std_path())
            .status()
            .with_context(|| format!("failed to open editor {editor}"))?;
        if !status.success() {
            return Err(anyhow!("editor exited with status {}", status));
        }
        Ok(())
    }

    fn remove_optional_file(&self, path: &Utf8Path) -> Result<()> {
        if !path.exists() {
            return Ok(());
        }
        fs::remove_file(path).with_context(|| format!("failed to remove {path}"))
    }

    pub async fn create_new<D>(
        &self,
        planning_request: &str,
        delegate: &mut D,
    ) -> Result<SpecSummary>
    where
        D: RunDelegate,
    {
        self.create_new_with_control(planning_request, RunControl::new(), delegate)
            .await
    }

    pub async fn create_new_with_control<D>(
        &self,
        planning_request: &str,
        control: RunControl,
        delegate: &mut D,
    ) -> Result<SpecSummary>
    where
        D: RunDelegate,
    {
        let paths = self.store.allocate_spec_pair()?;
        self.plan_paths(paths, planning_request, false, control, delegate)
            .await
    }

    pub async fn revise_target<D>(
        &self,
        target: &str,
        planning_request: &str,
        delegate: &mut D,
    ) -> Result<SpecSummary>
    where
        D: RunDelegate,
    {
        self.revise_target_with_control(target, planning_request, RunControl::new(), delegate)
            .await
    }

    pub async fn revise_target_with_control<D>(
        &self,
        target: &str,
        planning_request: &str,
        control: RunControl,
        delegate: &mut D,
    ) -> Result<SpecSummary>
    where
        D: RunDelegate,
    {
        let paths = self.store.resolve_target(target)?;
        self.plan_paths(paths, planning_request, false, control, delegate)
            .await
    }

    pub async fn replan_target<D>(
        &self,
        target: &str,
        planning_request: &str,
        delegate: &mut D,
    ) -> Result<SpecSummary>
    where
        D: RunDelegate,
    {
        self.replan_target_with_control(target, planning_request, RunControl::new(), delegate)
            .await
    }

    pub async fn replan_target_with_control<D>(
        &self,
        target: &str,
        planning_request: &str,
        control: RunControl,
        delegate: &mut D,
    ) -> Result<SpecSummary>
    where
        D: RunDelegate,
    {
        let paths = self.store.resolve_target(target)?;
        self.plan_paths(paths, planning_request, true, control, delegate)
            .await
    }

    pub async fn revise_progress_after_spec_edit_with_control<D>(
        &self,
        request: ProgressRevisionRequest,
        control: RunControl,
        delegate: &mut D,
    ) -> Result<SpecSummary>
    where
        D: RunDelegate,
    {
        self.store.ensure_ralph_dir()?;

        for iteration in 1..=self.config.planning_max_iterations {
            let planner_config = self.runner_config_for(RunnerMode::Plan, &control);
            if control.is_cancelled() {
                return Err(anyhow!("operation canceled"));
            }
            delegate
                .on_event(RunEvent::IterationStarted {
                    mode: RunnerMode::Plan,
                    iteration,
                    max_iterations: self.config.planning_max_iterations,
                })
                .await?;

            let current_spec_before = self.store.read_spec(&request.paths.spec_path)?;
            let past_spec_before = self.store.read_spec(&request.past_spec_path)?;
            let progress_before = self.store.read_progress(&request.paths.progress_path)?;
            let prompt = progress_revision_prompt(&ProgressRevisionPromptContext {
                previous_spec_path: request.past_spec_path.to_string(),
                current_spec_path: request.paths.spec_path.to_string(),
                progress_path: request.paths.progress_path.to_string(),
                diff_path: request.diff_path.to_string(),
            });

            let result = match self
                .execute_runner(
                    &planner_config,
                    RunnerInvocation {
                        prompt_text: prompt,
                        project_dir: self.project_dir.clone(),
                        mode: RunnerMode::Plan,
                        spec_path: request.paths.spec_path.clone(),
                        progress_path: request.paths.progress_path.clone(),
                        feedback_path: request.paths.feedback_path.clone(),
                    },
                    &control,
                    delegate,
                )
                .await
            {
                Ok(result) => result,
                Err(error) => {
                    if control.is_cancelled() {
                        return Err(anyhow!("operation canceled"));
                    }
                    self.store.append_controller_note(
                        &request.paths.progress_path,
                        &format!("progress revisor failed: {error}"),
                    )?;
                    delegate
                        .on_event(RunEvent::Note(format!(
                            "Progress revisor failed on iteration {iteration}: {error}"
                        )))
                        .await?;
                    continue;
                }
            };

            if result.exit_code != 0 {
                if control.is_cancelled() {
                    return Err(anyhow!("operation canceled"));
                }
                let note = format!("progress revisor exited with code {}", result.exit_code);
                self.store
                    .append_controller_note(&request.paths.progress_path, &note)?;
                delegate.on_event(RunEvent::Note(note)).await?;
                continue;
            }

            let current_spec_after = self.store.read_spec(&request.paths.spec_path)?;
            if current_spec_after != current_spec_before {
                self.store
                    .write_spec(&request.paths.spec_path, &current_spec_before)?;
                let note = "progress revisor must not modify the edited spec".to_owned();
                self.store
                    .append_controller_note(&request.paths.progress_path, &note)?;
                delegate.on_event(RunEvent::Note(note)).await?;
                continue;
            }

            let past_spec_after = self.store.read_spec(&request.past_spec_path)?;
            if past_spec_after != past_spec_before {
                self.store
                    .write_auxiliary(&request.past_spec_path, &past_spec_before)?;
                let note = "progress revisor must not modify the past spec snapshot".to_owned();
                self.store
                    .append_controller_note(&request.paths.progress_path, &note)?;
                delegate.on_event(RunEvent::Note(note)).await?;
                continue;
            }

            let marker = match parse_planning_marker_from_output(&result.output) {
                Ok(marker) => marker,
                Err(error) => {
                    let note = format!("progress revisor marker validation failed: {error}");
                    self.store
                        .append_controller_note(&request.paths.progress_path, &note)?;
                    delegate.on_event(RunEvent::Note(note)).await?;
                    continue;
                }
            };

            let progress_after = self.store.read_progress(&request.paths.progress_path)?;
            if progress_after.trim().is_empty() {
                let note = "progress revisor did not leave a non-empty progress file".to_owned();
                self.store
                    .append_controller_note(&request.paths.progress_path, &note)?;
                delegate.on_event(RunEvent::Note(note)).await?;
                continue;
            }

            if marker == ralph_core::PlanningMarker::Done {
                self.remove_optional_file(&request.past_spec_path)?;
                self.remove_optional_file(&request.diff_path)?;
                let summary = self.summary_for_paths(&request.paths)?;
                delegate
                    .on_event(RunEvent::Finished {
                        mode: RunnerMode::Plan,
                        completed: true,
                        summary: format!("Progress revised for {}", summary.spec_path),
                    })
                    .await?;
                return Ok(summary);
            }

            if progress_after == progress_before {
                let note = "progress revisor did not change progress".to_owned();
                self.store
                    .append_controller_note(&request.paths.progress_path, &note)?;
                delegate.on_event(RunEvent::Note(note)).await?;
            }
        }

        Err(anyhow!(
            "progress revisor exceeded {} iterations",
            self.config.planning_max_iterations
        ))
    }

    pub async fn run_target<D>(&self, target: &str, delegate: &mut D) -> Result<SpecSummary>
    where
        D: RunDelegate,
    {
        self.run_target_with_control(target, RunControl::new(), delegate)
            .await
    }

    pub async fn run_target_with_control<D>(
        &self,
        target: &str,
        control: RunControl,
        delegate: &mut D,
    ) -> Result<SpecSummary>
    where
        D: RunDelegate,
    {
        let paths = self.store.resolve_target(target)?;
        self.run_builder(paths, control, delegate).await
    }

    async fn plan_paths<D>(
        &self,
        paths: SpecPaths,
        planning_request: &str,
        reset: bool,
        control: RunControl,
        delegate: &mut D,
    ) -> Result<SpecSummary>
    where
        D: RunDelegate,
    {
        self.store.ensure_ralph_dir()?;
        if reset {
            self.store.delete_pair(&paths)?;
        }
        if self.ensure_planning_artifacts(&paths, planning_request)? {
            delegate
                .on_event(RunEvent::ArtifactsCreated {
                    spec_path: paths.spec_path.to_string(),
                    progress_path: paths.progress_path.to_string(),
                    feedback_path: paths.feedback_path.to_string(),
                })
                .await?;
        }

        let mut controller_warnings = Vec::new();
        for iteration in 1..=self.config.planning_max_iterations {
            let planner_config = self.runner_config_for(RunnerMode::Plan, &control);
            if control.is_cancelled() {
                return Err(anyhow!("operation canceled"));
            }
            delegate
                .on_event(RunEvent::IterationStarted {
                    mode: RunnerMode::Plan,
                    iteration,
                    max_iterations: self.config.planning_max_iterations,
                })
                .await?;

            let spec_before = self.store.read_spec(&paths.spec_path)?;
            let progress_before = self.store.read_progress(&paths.progress_path)?;
            let prompt = planning_prompt(&ralph_core::PlanningPromptContext {
                planning_request: planning_request.to_owned(),
                spec_path: paths.spec_path.to_string(),
                progress_path: paths.progress_path.to_string(),
                feedback_path: paths.feedback_path.to_string(),
                controller_warnings: controller_warnings.clone(),
                question_support: planner_config.question_support,
            });

            let result = match self
                .execute_runner(
                    &planner_config,
                    RunnerInvocation {
                        prompt_text: prompt,
                        project_dir: self.project_dir.clone(),
                        mode: RunnerMode::Plan,
                        spec_path: paths.spec_path.clone(),
                        progress_path: paths.progress_path.clone(),
                        feedback_path: paths.feedback_path.clone(),
                    },
                    &control,
                    delegate,
                )
                .await
            {
                Ok(result) => result,
                Err(error) => {
                    if control.is_cancelled() {
                        return Err(anyhow!("operation canceled"));
                    }
                    self.store.append_controller_note(
                        &paths.progress_path,
                        &format!("planning runner failed: {error}"),
                    )?;
                    delegate
                        .on_event(RunEvent::Note(format!(
                            "Planner runner failed on iteration {iteration}: {error}"
                        )))
                        .await?;
                    continue;
                }
            };

            if result.exit_code != 0 {
                if control.is_cancelled() {
                    return Err(anyhow!("operation canceled"));
                }
                let note = format!("planner exited with code {}", result.exit_code);
                self.store
                    .append_controller_note(&paths.progress_path, &note)?;
                delegate.on_event(RunEvent::Note(note)).await?;
                continue;
            }

            let asked_clarification = if let Some(request) =
                parse_clarification_request(&result.output)
            {
                let answer = delegate.ask_clarification(request.clone()).await?;
                let Some(answer) = answer else {
                    return Err(anyhow!("planning canceled during clarification"));
                };
                self.store.append_feedback_clarification(
                    &paths.feedback_path,
                    &request,
                    &answer,
                )?;
                true
            } else {
                if result.output.contains("<ralph-question>") {
                    let note = "planner emitted clarification tags but no valid clarification block could be parsed".to_owned();
                    self.store
                        .append_controller_note(&paths.progress_path, &note)?;
                    delegate.on_event(RunEvent::Note(note)).await?;
                }
                false
            };
            if asked_clarification {
                continue;
            }

            if control.is_cancelled() {
                return Err(anyhow!("operation canceled"));
            }

            let spec_after = self.store.read_spec(&paths.spec_path)?;
            let progress_after = self.store.read_progress(&paths.progress_path)?;

            if spec_after.trim().is_empty() {
                let note = "planner did not produce a non-empty spec".to_owned();
                self.store
                    .append_controller_note(&paths.progress_path, &note)?;
                delegate.on_event(RunEvent::Note(note)).await?;
                continue;
            }
            if progress_after.trim().is_empty() {
                let note = "planner did not produce a non-empty progress file".to_owned();
                self.store
                    .append_controller_note(&paths.progress_path, &note)?;
                delegate.on_event(RunEvent::Note(note)).await?;
                continue;
            }

            let marker = match parse_planning_marker_from_output(&result.output) {
                Ok(marker) => marker,
                Err(error) => {
                    let note = format!("planner marker validation failed: {error}");
                    self.store
                        .append_controller_note(&paths.progress_path, &note)?;
                    delegate.on_event(RunEvent::Note(note)).await?;
                    continue;
                }
            };

            if marker == ralph_core::PlanningMarker::Done {
                let summary = self.summary_for_paths(&paths)?;
                delegate
                    .on_event(RunEvent::Finished {
                        mode: RunnerMode::Plan,
                        completed: true,
                        summary: format!("Planning complete for {}", summary.spec_path),
                    })
                    .await?;
                return Ok(summary);
            }

            if spec_after == spec_before && progress_after == progress_before {
                let note = "planner did not change spec or progress".to_owned();
                self.store
                    .append_controller_note(&paths.progress_path, &note)?;
                delegate.on_event(RunEvent::Note(note)).await?;
                if marker == ralph_core::PlanningMarker::Continue {
                    let warning = "System warning: in the previous planning iteration you emitted CONTINUE without changing spec/progress and without asking clarification. This is irrational. In the next iteration you must either modify the plan artifacts, ask a clarification question, or emit DONE.".to_owned();
                    if !controller_warnings.contains(&warning) {
                        controller_warnings.push(warning);
                    }
                }
                continue;
            }
        }

        Err(anyhow!(
            "planning exceeded {} iterations",
            self.config.planning_max_iterations
        ))
    }

    fn ensure_planning_artifacts(&self, paths: &SpecPaths, planning_request: &str) -> Result<bool> {
        let mut created = false;
        if !paths.spec_path.exists() {
            self.store
                .write_spec(&paths.spec_path, &initial_spec_stub(planning_request))?;
            created = true;
        }
        if !paths.progress_path.exists() {
            self.store.write_progress(&paths.progress_path, "")?;
            created = true;
        }
        if !paths.feedback_path.exists() {
            self.store.write_feedback(
                &paths.feedback_path,
                &ArtifactStore::default_feedback_contents(),
            )?;
            created = true;
        }
        Ok(created)
    }

    async fn run_builder<D>(
        &self,
        paths: SpecPaths,
        control: RunControl,
        delegate: &mut D,
    ) -> Result<SpecSummary>
    where
        D: RunDelegate,
    {
        self.store.ensure_ralph_dir()?;

        let spec = self.store.read_spec(&paths.spec_path)?;
        if spec.trim().is_empty() {
            return Err(anyhow!("cannot run builder without a non-empty spec"));
        }
        if !paths.feedback_path.exists() {
            self.store.write_feedback(
                &paths.feedback_path,
                &ArtifactStore::default_feedback_contents(),
            )?;
        }

        for iteration in 1..=self.config.builder_max_iterations {
            let builder_config = self.runner_config_for(RunnerMode::Build, &control);
            if control.is_cancelled() {
                return Err(anyhow!("operation canceled"));
            }
            delegate
                .on_event(RunEvent::IterationStarted {
                    mode: RunnerMode::Build,
                    iteration,
                    max_iterations: self.config.builder_max_iterations,
                })
                .await?;

            let progress_before = self.store.read_progress(&paths.progress_path)?;
            let stripped = strip_persisted_promise_markers(&progress_before);
            if stripped != progress_before {
                self.store.write_progress(&paths.progress_path, &stripped)?;
            }

            let prompt = build_prompt(&BuildPromptContext {
                spec_path: paths.spec_path.to_string(),
                progress_path: paths.progress_path.to_string(),
                feedback_path: paths.feedback_path.to_string(),
            });

            let result = match self
                .execute_runner(
                    &builder_config,
                    RunnerInvocation {
                        prompt_text: prompt,
                        project_dir: self.project_dir.clone(),
                        mode: RunnerMode::Build,
                        spec_path: paths.spec_path.clone(),
                        progress_path: paths.progress_path.clone(),
                        feedback_path: paths.feedback_path.clone(),
                    },
                    &control,
                    delegate,
                )
                .await
            {
                Ok(result) => result,
                Err(error) => {
                    if control.is_cancelled() {
                        return Err(anyhow!("operation canceled"));
                    }
                    self.store.append_controller_note(
                        &paths.progress_path,
                        &format!("builder runner failed: {error}"),
                    )?;
                    delegate
                        .on_event(RunEvent::Note(format!(
                            "Builder runner failed on iteration {iteration}: {error}"
                        )))
                        .await?;
                    continue;
                }
            };

            if result.exit_code != 0 {
                if control.is_cancelled() {
                    return Err(anyhow!("operation canceled"));
                }
                let note = format!("builder exited with code {}", result.exit_code);
                self.store
                    .append_controller_note(&paths.progress_path, &note)?;
                delegate.on_event(RunEvent::Note(note)).await?;
                continue;
            }

            let marker = match parse_builder_marker_from_output(&result.output) {
                Ok(marker) => marker,
                Err(error) => {
                    let progress_after = self.store.read_progress(&paths.progress_path)?;
                    let unchanged_progress =
                        strip_persisted_promise_markers(&progress_after) == stripped;
                    let note = if unchanged_progress {
                        format!(
                            "builder marker validation failed: {error}; builder exited without updating progress"
                        )
                    } else {
                        format!("builder marker validation failed: {error}")
                    };
                    self.store
                        .append_controller_note(&paths.progress_path, &note)?;
                    delegate.on_event(RunEvent::Note(note)).await?;
                    continue;
                }
            };

            let progress_after = self.store.read_progress(&paths.progress_path)?;
            if progress_after.trim().is_empty() {
                let note = "builder did not leave a progress file".to_owned();
                self.store
                    .append_controller_note(&paths.progress_path, &note)?;
                delegate.on_event(RunEvent::Note(note)).await?;
                continue;
            }

            if marker == BuilderMarker::Done {
                self.store.persist_done_marker(&paths.progress_path)?;
                let summary = self.summary_for_paths(&paths)?;
                delegate
                    .on_event(RunEvent::Finished {
                        mode: RunnerMode::Build,
                        completed: true,
                        summary: format!("Build complete for {}", summary.spec_path),
                    })
                    .await?;
                return Ok(summary);
            }
        }

        Err(anyhow!(
            "builder exceeded {} iterations",
            self.config.builder_max_iterations
        ))
    }

    fn summary_for_paths(&self, paths: &SpecPaths) -> Result<SpecSummary> {
        let review = self.store.review(paths)?;
        let state = self.store.state_for_paths(paths)?;
        debug!(spec = %paths.spec_path, ?state, "built spec summary");
        Ok(SpecSummary {
            spec_path: review.spec_path,
            progress_path: review.progress_path,
            feedback_path: review.feedback_path,
            state,
            spec_preview: preview(&review.spec_contents),
            progress_preview: preview(&review.progress_contents),
            feedback_preview: preview(&review.feedback_contents),
        })
    }

    fn runner_config_for(&self, mode: RunnerMode, control: &RunControl) -> RunnerConfig {
        if let Some(agent) = control.coding_agent() {
            RunnerConfig::for_agent(agent)
        } else {
            match mode {
                RunnerMode::Plan => self.config.planner.clone(),
                RunnerMode::Build => self.config.builder.clone(),
            }
        }
    }

    async fn execute_runner<D>(
        &self,
        config: &ralph_core::RunnerConfig,
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

fn preview(contents: &str) -> String {
    let compact = contents
        .lines()
        .skip_while(|line| line.trim().is_empty())
        .map(str::trim_end)
        .take(12)
        .collect::<Vec<_>>();
    if compact.is_empty() {
        "<empty>".to_owned()
    } else {
        compact.join("\n")
    }
}

fn initial_spec_stub(planning_request: &str) -> String {
    let request = planning_request.trim();
    format!(
        "# Goal\nInitial planning request: {request}\n\n# User Requirements And Constraints\nInitial request captured before full planning:\n- {request}\n\n# Non-Goals\nTo be defined during planning.\n\n# Proposed Design\nTo be defined during planning.\n\n# Implementation Plan\nTo be defined during planning.\n\n# Acceptance Criteria\nTo be defined during planning.\n\n# Risks\nPlanning was interrupted before a full spec was produced.\n\n# Open Questions\nSee the feedback file for clarification history and unresolved questions.\n"
    )
}

fn render_spec_diff(
    previous_spec_path: &Utf8Path,
    current_spec_path: &Utf8Path,
    previous_spec: &str,
    current_spec: &str,
) -> String {
    TextDiff::from_lines(previous_spec, current_spec)
        .unified_diff()
        .context_radius(3)
        .header(previous_spec_path.as_str(), current_spec_path.as_str())
        .to_string()
}

#[derive(Default)]
pub struct ConsoleDelegate;

#[async_trait]
impl RunDelegate for ConsoleDelegate {
    async fn on_event(&mut self, event: RunEvent) -> Result<()> {
        match event {
            RunEvent::ArtifactsCreated {
                spec_path,
                progress_path,
                feedback_path,
            } => {
                println!(
                    "artifacts created:\n- spec: {spec_path}\n- progress: {progress_path}\n- feedback: {feedback_path}"
                );
            }
            RunEvent::IterationStarted {
                mode,
                iteration,
                max_iterations,
            } => {
                println!(
                    "{}",
                    format_iteration_banner(mode, iteration, max_iterations)
                );
            }
            RunEvent::Output(chunk) => print!("{chunk}"),
            RunEvent::Note(note) => eprintln!("note: {note}"),
            RunEvent::Finished { summary, .. } => println!("{summary}"),
        }
        Ok(())
    }

    async fn ask_clarification(
        &mut self,
        request: ClarificationRequest,
    ) -> Result<Option<ClarificationAnswer>> {
        println!("\nClarification required:\n{}\n", request.question);
        for (index, option) in request.options.iter().enumerate() {
            println!("{}. {} - {}", index + 1, option.label, option.description);
        }
        loop {
            println!("Enter a number, free-form answer, or /quit to abort:");

            let mut input = String::new();
            std::io::stdin()
                .read_line(&mut input)
                .context("failed to read clarification answer")?;
            let input = input.trim();
            if input.eq_ignore_ascii_case("/quit") {
                return Ok(None);
            }
            if input.is_empty() {
                println!("Answer required. Type a choice, free-form answer, or /quit.");
                continue;
            }
            if let Ok(index) = input.parse::<usize>() {
                if let Some(option) = request.options.get(index.saturating_sub(1)) {
                    return Ok(Some(ClarificationAnswer {
                        text: option.label.clone(),
                        used_option_selection: true,
                    }));
                }
            }
            return Ok(Some(ClarificationAnswer {
                text: input.to_owned(),
                used_option_selection: false,
            }));
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        sync::{Arc, Mutex},
        time::Duration,
    };

    use super::*;
    use anyhow::anyhow;
    use ralph_core::{RunnerResult, WorkflowState};

    #[derive(Clone)]
    struct ScriptedRunner {
        steps: Arc<Mutex<Vec<Box<dyn Fn(&RunnerInvocation) -> RunnerResult + Send + Sync>>>>,
    }

    impl ScriptedRunner {
        fn new(steps: Vec<Box<dyn Fn(&RunnerInvocation) -> RunnerResult + Send + Sync>>) -> Self {
            Self {
                steps: Arc::new(Mutex::new(steps)),
            }
        }
    }

    #[async_trait]
    impl RunnerAdapter for ScriptedRunner {
        async fn run(
            &self,
            _config: &ralph_core::RunnerConfig,
            invocation: RunnerInvocation,
            _control: &RunControl,
            stream: Option<tokio::sync::mpsc::UnboundedSender<RunnerStreamEvent>>,
        ) -> Result<RunnerResult> {
            let next = self.steps.lock().unwrap().remove(0);
            let result = next(&invocation);
            if let Some(stream) = stream {
                if !result.output.is_empty() {
                    let _ = stream.send(RunnerStreamEvent::Output(result.output.clone()));
                }
            }
            Ok(result)
        }
    }

    struct TestDelegate {
        answers: Vec<Option<String>>,
        created: Vec<(String, String, String)>,
        notes: Vec<String>,
        output: Vec<String>,
    }

    #[async_trait]
    impl RunDelegate for TestDelegate {
        async fn on_event(&mut self, event: RunEvent) -> Result<()> {
            match event {
                RunEvent::ArtifactsCreated {
                    spec_path,
                    progress_path,
                    feedback_path,
                } => self.created.push((spec_path, progress_path, feedback_path)),
                RunEvent::Note(note) => self.notes.push(note),
                RunEvent::Output(chunk) => self.output.push(chunk),
                _ => {}
            }
            Ok(())
        }

        async fn ask_clarification(
            &mut self,
            _request: ClarificationRequest,
        ) -> Result<Option<ClarificationAnswer>> {
            Ok(self.answers.remove(0).map(|text| ClarificationAnswer {
                text,
                used_option_selection: false,
            }))
        }
    }

    fn app(runner: ScriptedRunner) -> (tempfile::TempDir, RalphApp<ScriptedRunner>) {
        let temp = tempfile::TempDir::new().unwrap();
        let project_dir = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();
        let mut config = AppConfig::default();
        config.planning_max_iterations = 3;
        config.builder_max_iterations = 3;
        (temp, RalphApp::new(project_dir, config, runner))
    }

    fn sample_spec(suffix: &str) -> String {
        format!(
            "# Goal\nGoal {suffix}\n\n# User Requirements And Constraints\nRequirements {suffix}\n\n# Non-Goals\nNon-goals {suffix}\n\n# Proposed Design\nDesign {suffix}\n\n# Implementation Plan\nPlan {suffix}\n\n# Acceptance Criteria\nAcceptance {suffix}\n\n# Risks\nRisks {suffix}\n\n# Open Questions\nQuestions {suffix}\n"
        )
    }

    #[tokio::test]
    async fn planning_retries_on_invalid_marker() {
        let runner = ScriptedRunner::new(vec![
            Box::new(|invocation| {
                let spec = fs::read_to_string(&invocation.spec_path).unwrap();
                assert!(spec.contains("# Goal"));
                assert!(spec.contains("Initial planning request: Implement feature"));
                assert!(
                    fs::read_to_string(&invocation.progress_path)
                        .unwrap()
                        .is_empty()
                );
                assert!(
                    fs::read_to_string(&invocation.feedback_path)
                        .unwrap()
                        .contains("<RECENT-USER-FEEDBACK>")
                );
                fs::write(&invocation.spec_path, sample_spec("A")).unwrap();
                fs::write(&invocation.progress_path, "Task 1\n").unwrap();
                RunnerResult {
                    output: "not a marker\n".to_owned(),
                    exit_code: 0,
                }
            }),
            Box::new(|invocation| {
                fs::write(&invocation.spec_path, sample_spec("D")).unwrap();
                fs::write(&invocation.progress_path, "Task 1\nTask 2\n").unwrap();
                RunnerResult {
                    output: "<plan-promise>DONE</plan-promise>\n".to_owned(),
                    exit_code: 0,
                }
            }),
        ]);

        let (_temp, app) = app(runner);
        let mut delegate = TestDelegate {
            answers: vec![],
            created: vec![],
            notes: vec![],
            output: vec![],
        };
        let summary = app
            .create_new("Implement feature", &mut delegate)
            .await
            .unwrap();
        assert_eq!(summary.state, WorkflowState::Planned);
        assert_eq!(delegate.created.len(), 1);
        assert!(delegate.created[0].0.ends_with(".md"));
        assert!(delegate.created[0].2.ends_with(".txt"));
        assert!(
            delegate
                .notes
                .iter()
                .any(|note| note.contains("planner marker validation failed"))
        );
    }

    #[tokio::test]
    async fn builder_persists_done_marker() {
        let runner = ScriptedRunner::new(vec![Box::new(|invocation| {
            assert!(invocation.prompt_text.contains("- feedback:"));
            fs::write(&invocation.progress_path, "Task done\n").unwrap();
            RunnerResult {
                output: "<promise>DONE</promise>\n".to_owned(),
                exit_code: 0,
            }
        })]);

        let (_temp, app) = app(runner);
        let paths = app.resolve_target("alpha").unwrap();
        fs::create_dir_all(paths.spec_path.parent().unwrap()).unwrap();
        fs::write(&paths.spec_path, sample_spec("C")).unwrap();
        fs::write(
            &paths.progress_path,
            "Task 1\n<promise>CONTINUE</promise>\n",
        )
        .unwrap();

        let mut delegate = TestDelegate {
            answers: vec![],
            created: vec![],
            notes: vec![],
            output: vec![],
        };
        let summary = app.run_target("alpha", &mut delegate).await.unwrap();
        assert_eq!(summary.state, WorkflowState::Completed);
        let progress = fs::read_to_string(&paths.progress_path).unwrap();
        assert_eq!(progress, "Task done\n<promise>DONE</promise>\n");
    }

    #[tokio::test]
    async fn planning_preserves_partial_artifacts_on_cancel() {
        let runner = ScriptedRunner::new(vec![Box::new(|invocation| {
            fs::write(&invocation.spec_path, sample_spec("C")).unwrap();
            fs::write(&invocation.progress_path, "Task 1\n").unwrap();
            RunnerResult {
                output: "<ralph-question>{\"question\":\"Which?\",\"options\":[]}</ralph-question>"
                    .to_owned(),
                exit_code: 0,
            }
        })]);

        let (_temp, app) = app(runner);
        let mut delegate = TestDelegate {
            answers: vec![None],
            created: vec![],
            notes: vec![],
            output: vec![],
        };

        assert!(
            app.create_new("Implement feature", &mut delegate)
                .await
                .is_err()
        );
        let specs = app.list_specs().unwrap();
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].state, WorkflowState::Planned);
    }

    #[tokio::test]
    async fn planning_keeps_stubbed_new_spec_visible_during_clarification() {
        let runner = ScriptedRunner::new(vec![Box::new(|invocation| {
            let spec = fs::read_to_string(&invocation.spec_path).unwrap();
            assert!(spec.contains("# Goal"));
            assert!(spec.contains("Initial planning request: Implement feature"));
            assert!(
                fs::read_to_string(&invocation.progress_path)
                    .unwrap()
                    .is_empty()
            );
            RunnerResult {
                output: "<ralph-question>{\"question\":\"Which?\",\"options\":[]}</ralph-question>"
                    .to_owned(),
                exit_code: 0,
            }
        })]);

        let (_temp, app) = app(runner);
        let mut delegate = TestDelegate {
            answers: vec![None],
            created: vec![],
            notes: vec![],
            output: vec![],
        };

        assert!(
            app.create_new("Implement feature", &mut delegate)
                .await
                .is_err()
        );

        let specs = app.list_specs().unwrap();
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].state, WorkflowState::Planned);
        assert!(specs[0].spec_preview.contains("Initial planning request"));
        assert_eq!(specs[0].progress_preview, "<empty>");
        assert_eq!(delegate.created.len(), 1);
        assert!(
            fs::read_to_string(&delegate.created[0].2)
                .unwrap()
                .contains("None.")
        );
    }

    #[tokio::test]
    async fn interrupted_planning_leaves_a_non_empty_runnable_stub_spec() {
        let runner = ScriptedRunner::new(vec![
            Box::new(|invocation| {
                let spec = fs::read_to_string(&invocation.spec_path).unwrap();
                assert!(spec.contains("Initial planning request: Implement feature"));
                RunnerResult {
                    output:
                        "<ralph-question>{\"question\":\"Which?\",\"options\":[]}</ralph-question>"
                            .to_owned(),
                    exit_code: 0,
                }
            }),
            Box::new(|invocation| {
                let spec = fs::read_to_string(&invocation.spec_path).unwrap();
                assert!(spec.contains("Initial planning request: Implement feature"));
                fs::write(&invocation.progress_path, "Task 1\n").unwrap();
                RunnerResult {
                    output: "<promise>DONE</promise>\n".to_owned(),
                    exit_code: 0,
                }
            }),
        ]);

        let (_temp, app) = app(runner);
        let mut planning_delegate = TestDelegate {
            answers: vec![None],
            created: vec![],
            notes: vec![],
            output: vec![],
        };

        assert!(
            app.create_new("Implement feature", &mut planning_delegate)
                .await
                .is_err()
        );

        let target = planning_delegate.created[0].0.clone();
        let mut builder_delegate = TestDelegate {
            answers: vec![],
            created: vec![],
            notes: vec![],
            output: vec![],
        };

        let summary = app
            .run_target(&target, &mut builder_delegate)
            .await
            .unwrap();
        assert_eq!(summary.state, WorkflowState::Completed);
    }

    #[tokio::test]
    async fn planning_notes_malformed_clarification_blocks() {
        let runner = ScriptedRunner::new(vec![Box::new(|invocation| {
            fs::write(&invocation.spec_path, sample_spec("C")).unwrap();
            fs::write(&invocation.progress_path, "Task 1\n").unwrap();
            RunnerResult {
                output:
                    "<ralph-question>{oops}</ralph-question>\n<plan-promise>DONE</plan-promise>\n"
                        .to_owned(),
                exit_code: 0,
            }
        })]);

        let (_temp, app) = app(runner);
        let mut delegate = TestDelegate {
            answers: vec![],
            created: vec![],
            notes: vec![],
            output: vec![],
        };

        let summary = app
            .create_new("Implement feature", &mut delegate)
            .await
            .unwrap();
        assert_eq!(summary.state, WorkflowState::Planned);
        assert!(
            delegate
                .notes
                .iter()
                .any(|note| { note.contains("no valid clarification block could be parsed") })
        );
    }

    #[tokio::test]
    async fn planning_persists_clarification_history_in_feedback_file() {
        let runner = ScriptedRunner::new(vec![
            Box::new(|invocation| {
                fs::write(&invocation.spec_path, sample_spec("C")).unwrap();
                fs::write(&invocation.progress_path, "Task 1\n").unwrap();
                RunnerResult {
                    output: "<ralph-question>{\"question\":\"Which database should the plan assume?\",\"options\":[{\"label\":\"Postgres\",\"description\":\"Use PostgreSQL\"}]}</ralph-question>".to_owned(),
                    exit_code: 0,
                }
            }),
            Box::new(|invocation| {
                assert!(invocation.prompt_text.contains("- feedback:"));
                assert!(
                    !invocation
                        .prompt_text
                        .contains("Q: Which database should the plan assume?")
                );
                let feedback = fs::read_to_string(&invocation.feedback_path).unwrap();
                assert!(feedback.contains("<RECENT-USER-FEEDBACK>"));
                assert!(feedback.contains("Q: Which database should the plan assume?"));
                assert!(feedback.contains("A: Postgres"));
                assert!(feedback.contains("<OLDER-USER-FEEDBACK>\nNone."));
                fs::write(&invocation.spec_path, sample_spec("D")).unwrap();
                fs::write(&invocation.progress_path, "Task 1\nTask 2\n").unwrap();
                RunnerResult {
                    output: "<plan-promise>DONE</plan-promise>\n".to_owned(),
                    exit_code: 0,
                }
            }),
        ]);

        let (_temp, app) = app(runner);
        let mut delegate = TestDelegate {
            answers: vec![Some("Postgres".to_owned())],
            created: vec![],
            notes: vec![],
            output: vec![],
        };

        let summary = app
            .create_new("Implement feature", &mut delegate)
            .await
            .unwrap();
        assert_eq!(summary.state, WorkflowState::Planned);
    }

    #[tokio::test]
    async fn planning_done_succeeds_even_without_artifact_changes_in_final_pass() {
        let runner = ScriptedRunner::new(vec![
            Box::new(|invocation| {
                fs::write(&invocation.spec_path, sample_spec("C")).unwrap();
                fs::write(&invocation.progress_path, "Task 1\n").unwrap();
                RunnerResult {
                    output: "<plan-promise>CONTINUE</plan-promise>\n".to_owned(),
                    exit_code: 0,
                }
            }),
            Box::new(|_invocation| RunnerResult {
                output: "<plan-promise>DONE</plan-promise>\n".to_owned(),
                exit_code: 0,
            }),
        ]);

        let (_temp, app) = app(runner);
        let mut delegate = TestDelegate {
            answers: vec![],
            created: vec![],
            notes: vec![],
            output: vec![],
        };

        let summary = app
            .create_new("Implement feature", &mut delegate)
            .await
            .unwrap();
        assert_eq!(summary.state, WorkflowState::Planned);
        assert!(
            !delegate
                .notes
                .iter()
                .any(|note| note.contains("planner did not change spec or progress"))
        );
    }

    #[tokio::test]
    async fn planning_warns_after_continue_without_changes_or_questions() {
        let runner = ScriptedRunner::new(vec![
            Box::new(|invocation| {
                fs::write(&invocation.spec_path, sample_spec("C")).unwrap();
                fs::write(&invocation.progress_path, "Task 1\n").unwrap();
                RunnerResult {
                    output: "<plan-promise>CONTINUE</plan-promise>\n".to_owned(),
                    exit_code: 0,
                }
            }),
            Box::new(|invocation| {
                assert!(invocation.prompt_text.contains(
                    "System warning: in the previous planning iteration you emitted CONTINUE without changing spec/progress and without asking clarification."
                ));
                fs::write(&invocation.spec_path, sample_spec("C")).unwrap();
                fs::write(&invocation.progress_path, "Task 1\nTask 2\n").unwrap();
                RunnerResult {
                    output: "<plan-promise>DONE</plan-promise>\n".to_owned(),
                    exit_code: 0,
                }
            }),
        ]);

        let (_temp, app) = app(runner);
        let paths = app.resolve_target("alpha").unwrap();
        fs::create_dir_all(paths.spec_path.parent().unwrap()).unwrap();
        fs::write(&paths.spec_path, sample_spec("C")).unwrap();
        fs::write(&paths.progress_path, "Task 1\n").unwrap();

        let mut delegate = TestDelegate {
            answers: vec![],
            created: vec![],
            notes: vec![],
            output: vec![],
        };

        let summary = app
            .revise_target("alpha", "Implement feature", &mut delegate)
            .await
            .unwrap();
        assert_eq!(summary.state, WorkflowState::Planned);
        assert!(
            delegate
                .notes
                .iter()
                .any(|note| note.contains("planner did not change spec or progress"))
        );
    }

    struct BlockingRunner;

    #[async_trait]
    impl RunnerAdapter for BlockingRunner {
        async fn run(
            &self,
            _config: &ralph_core::RunnerConfig,
            _invocation: RunnerInvocation,
            control: &RunControl,
            stream: Option<tokio::sync::mpsc::UnboundedSender<RunnerStreamEvent>>,
        ) -> Result<RunnerResult> {
            if let Some(stream) = stream {
                let _ = stream.send(RunnerStreamEvent::Output("started\n".to_owned()));
            }
            loop {
                if control.is_cancelled() {
                    return Err(anyhow!("runner canceled"));
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }
    }

    #[tokio::test]
    async fn builder_cancellation_stops_runner() {
        let temp = tempfile::TempDir::new().unwrap();
        let project_dir = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();
        let mut config = AppConfig::default();
        config.builder_max_iterations = 2;
        let app = RalphApp::new(project_dir, config, BlockingRunner);

        let paths = app.resolve_target("alpha").unwrap();
        fs::create_dir_all(paths.spec_path.parent().unwrap()).unwrap();
        fs::write(&paths.spec_path, sample_spec("C")).unwrap();
        fs::write(&paths.progress_path, "Task 1\n").unwrap();

        let control = RunControl::new();
        let cancel = control.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(30)).await;
            cancel.cancel();
        });

        let mut delegate = TestDelegate {
            answers: vec![],
            created: vec![],
            notes: vec![],
            output: vec![],
        };

        let error = app
            .run_target_with_control("alpha", control, &mut delegate)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("operation canceled"));
        assert!(
            delegate
                .output
                .iter()
                .any(|chunk| chunk.contains("started"))
        );
    }

    #[test]
    fn iteration_banner_is_visually_distinct() {
        let banner = format_iteration_banner(RunnerMode::Plan, 2, 8);
        assert!(banner.starts_with('\n'));
        assert!(banner.contains("PLAN ITERATION 2/8"));
        assert_eq!(banner.lines().count(), 4);
    }
}
