use std::{env, process::Command};

use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use camino::{Utf8Path, Utf8PathBuf};
use ralph_core::{
    AppConfig, ArtifactStore, BuildPromptContext, BuilderMarker, ClarificationExchange,
    ClarificationRequest, ReviewData, RunControl, RunnerInvocation, RunnerMode, SpecPaths,
    SpecSummary, build_prompt, parse_builder_marker_from_output, parse_clarification_request,
    parse_planning_marker_from_output, planning_prompt, strip_persisted_promise_markers,
};
use ralph_runner::{CommandRunner, RunnerAdapter, RunnerStreamEvent};
use tracing::debug;

#[derive(Debug, Clone)]
pub enum RunEvent {
    IterationStarted {
        mode: RunnerMode,
        iteration: usize,
        max_iterations: usize,
    },
    Stdout(String),
    Stderr(String),
    Note(String),
    Finished {
        mode: RunnerMode,
        completed: bool,
        summary: String,
    },
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

    async fn ask_clarification(&mut self, request: ClarificationRequest) -> Result<Option<String>>;
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
        let paths = self.store.resolve_target(target)?;
        self.store.ensure_ralph_dir()?;
        if !paths.spec_path.exists() {
            self.store.write_spec(&paths.spec_path, "")?;
        }

        let editor = self
            .config
            .editor_override
            .clone()
            .or_else(|| env::var("VISUAL").ok())
            .or_else(|| env::var("EDITOR").ok())
            .unwrap_or_else(|| "vi".to_owned());

        let status = Command::new(&editor)
            .arg(paths.spec_path.as_std_path())
            .status()
            .with_context(|| format!("failed to open editor {editor}"))?;
        if !status.success() {
            return Err(anyhow!("editor exited with status {}", status));
        }
        Ok(())
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

        let mut clarification_history = Vec::new();
        let mut controller_warnings = Vec::new();
        for iteration in 1..=self.config.planning_max_iterations {
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
                clarification_history: clarification_history.clone(),
                controller_warnings: controller_warnings.clone(),
                question_support: self.config.planner.question_support,
            });

            let result = match self
                .execute_runner(
                    &self.config.planner,
                    RunnerInvocation {
                        prompt_text: prompt,
                        project_dir: self.project_dir.clone(),
                        mode: RunnerMode::Plan,
                        spec_path: paths.spec_path.clone(),
                        progress_path: paths.progress_path.clone(),
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

            let asked_clarification =
                if let Some(request) = parse_clarification_request(&result.stdout) {
                    let question = request.question.clone();
                    let answer = delegate.ask_clarification(request).await?;
                    let Some(answer) = answer else {
                        return Err(anyhow!("planning canceled during clarification"));
                    };
                    clarification_history.push(ClarificationExchange { question, answer });
                    true
                } else {
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

            let marker = match parse_planning_marker_from_output(&result.stdout) {
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

        for iteration in 1..=self.config.builder_max_iterations {
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
            });

            let result = match self
                .execute_runner(
                    &self.config.builder,
                    RunnerInvocation {
                        prompt_text: prompt,
                        project_dir: self.project_dir.clone(),
                        mode: RunnerMode::Build,
                        spec_path: paths.spec_path.clone(),
                        progress_path: paths.progress_path.clone(),
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

            let marker = match parse_builder_marker_from_output(&result.stdout) {
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
            state,
            spec_preview: preview(&review.spec_contents),
            progress_preview: preview(&review.progress_contents),
        })
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
        RunnerStreamEvent::Stdout(chunk) => delegate.on_event(RunEvent::Stdout(chunk)).await,
        RunnerStreamEvent::Stderr(chunk) => delegate.on_event(RunEvent::Stderr(chunk)).await,
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

#[derive(Default)]
pub struct ConsoleDelegate;

#[async_trait]
impl RunDelegate for ConsoleDelegate {
    async fn on_event(&mut self, event: RunEvent) -> Result<()> {
        match event {
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
            RunEvent::Stdout(chunk) => print!("{chunk}"),
            RunEvent::Stderr(chunk) => eprint!("{chunk}"),
            RunEvent::Note(note) => eprintln!("note: {note}"),
            RunEvent::Finished { summary, .. } => println!("{summary}"),
        }
        Ok(())
    }

    async fn ask_clarification(&mut self, request: ClarificationRequest) -> Result<Option<String>> {
        println!("\nClarification required:\n{}\n", request.question);
        for (index, option) in request.options.iter().enumerate() {
            println!("{}. {} - {}", index + 1, option.label, option.description);
        }
        println!("Enter a number, free-form answer, or leave blank to cancel:");

        let mut input = String::new();
        std::io::stdin()
            .read_line(&mut input)
            .context("failed to read clarification answer")?;
        let input = input.trim();
        if input.is_empty() {
            return Ok(None);
        }
        if let Ok(index) = input.parse::<usize>() {
            if let Some(option) = request.options.get(index.saturating_sub(1)) {
                return Ok(Some(option.label.clone()));
            }
        }
        Ok(Some(input.to_owned()))
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
                if !result.stdout.is_empty() {
                    let _ = stream.send(RunnerStreamEvent::Stdout(result.stdout.clone()));
                }
                if !result.stderr.is_empty() {
                    let _ = stream.send(RunnerStreamEvent::Stderr(result.stderr.clone()));
                }
            }
            Ok(result)
        }
    }

    struct TestDelegate {
        answers: Vec<Option<String>>,
        notes: Vec<String>,
        stdout: Vec<String>,
        stderr: Vec<String>,
    }

    #[async_trait]
    impl RunDelegate for TestDelegate {
        async fn on_event(&mut self, event: RunEvent) -> Result<()> {
            match event {
                RunEvent::Note(note) => self.notes.push(note),
                RunEvent::Stdout(chunk) => self.stdout.push(chunk),
                RunEvent::Stderr(chunk) => self.stderr.push(chunk),
                _ => {}
            }
            Ok(())
        }

        async fn ask_clarification(
            &mut self,
            _request: ClarificationRequest,
        ) -> Result<Option<String>> {
            Ok(self.answers.remove(0))
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
                fs::write(&invocation.spec_path, sample_spec("A")).unwrap();
                fs::write(&invocation.progress_path, "Task 1\n").unwrap();
                RunnerResult {
                    stdout: "not a marker\n".to_owned(),
                    stderr: String::new(),
                    exit_code: 0,
                }
            }),
            Box::new(|invocation| {
                fs::write(&invocation.spec_path, sample_spec("D")).unwrap();
                fs::write(&invocation.progress_path, "Task 1\nTask 2\n").unwrap();
                RunnerResult {
                    stdout: "<plan-promise>DONE</plan-promise>\n".to_owned(),
                    stderr: String::new(),
                    exit_code: 0,
                }
            }),
        ]);

        let (_temp, app) = app(runner);
        let mut delegate = TestDelegate {
            answers: vec![],
            notes: vec![],
            stdout: vec![],
            stderr: vec![],
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
                .any(|note| note.contains("planner marker validation failed"))
        );
    }

    #[tokio::test]
    async fn builder_persists_done_marker() {
        let runner = ScriptedRunner::new(vec![Box::new(|invocation| {
            fs::write(&invocation.progress_path, "Task done\n").unwrap();
            RunnerResult {
                stdout: "<promise>DONE</promise>\n".to_owned(),
                stderr: String::new(),
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
            notes: vec![],
            stdout: vec![],
            stderr: vec![],
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
                stdout: "<ralph-question>{\"question\":\"Which?\",\"options\":[]}</ralph-question>"
                    .to_owned(),
                stderr: String::new(),
                exit_code: 0,
            }
        })]);

        let (_temp, app) = app(runner);
        let mut delegate = TestDelegate {
            answers: vec![None],
            notes: vec![],
            stdout: vec![],
            stderr: vec![],
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
    async fn planning_ignores_malformed_clarification_blocks() {
        let runner = ScriptedRunner::new(vec![Box::new(|invocation| {
            fs::write(&invocation.spec_path, sample_spec("C")).unwrap();
            fs::write(&invocation.progress_path, "Task 1\n").unwrap();
            RunnerResult {
                stdout:
                    "<ralph-question>{oops}</ralph-question>\n<plan-promise>DONE</plan-promise>\n"
                        .to_owned(),
                stderr: String::new(),
                exit_code: 0,
            }
        })]);

        let (_temp, app) = app(runner);
        let mut delegate = TestDelegate {
            answers: vec![],
            notes: vec![],
            stdout: vec![],
            stderr: vec![],
        };

        let summary = app
            .create_new("Implement feature", &mut delegate)
            .await
            .unwrap();
        assert_eq!(summary.state, WorkflowState::Planned);
        assert!(delegate.notes.is_empty());
    }

    #[tokio::test]
    async fn planning_reinjects_prior_clarification_question_and_answer() {
        let runner = ScriptedRunner::new(vec![
            Box::new(|invocation| {
                fs::write(&invocation.spec_path, sample_spec("C")).unwrap();
                fs::write(&invocation.progress_path, "Task 1\n").unwrap();
                RunnerResult {
                    stdout: "<ralph-question>{\"question\":\"Which database should the plan assume?\",\"options\":[{\"label\":\"Postgres\",\"description\":\"Use PostgreSQL\"}]}</ralph-question>".to_owned(),
                    stderr: String::new(),
                    exit_code: 0,
                }
            }),
            Box::new(|invocation| {
                assert!(
                    invocation
                        .prompt_text
                        .contains("Recent feedback to shape the plan:")
                );
                assert!(
                    invocation
                        .prompt_text
                        .contains("This is the most recent authoritative user guidance collected in the previous planning iteration.")
                );
                assert!(
                    invocation
                        .prompt_text
                        .contains("Older feedbacks from past iterations:\nThese older clarifications remain authoritative unless superseded by newer feedback above.\nNone.")
                );
                assert!(
                    invocation
                        .prompt_text
                        .contains("Q: Which database should the plan assume?")
                );
                assert!(invocation.prompt_text.contains("A: Postgres"));
                fs::write(&invocation.spec_path, sample_spec("D")).unwrap();
                fs::write(&invocation.progress_path, "Task 1\nTask 2\n").unwrap();
                RunnerResult {
                    stdout: "<plan-promise>DONE</plan-promise>\n".to_owned(),
                    stderr: String::new(),
                    exit_code: 0,
                }
            }),
        ]);

        let (_temp, app) = app(runner);
        let mut delegate = TestDelegate {
            answers: vec![Some("Postgres".to_owned())],
            notes: vec![],
            stdout: vec![],
            stderr: vec![],
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
                    stdout: "<plan-promise>CONTINUE</plan-promise>\n".to_owned(),
                    stderr: String::new(),
                    exit_code: 0,
                }
            }),
            Box::new(|_invocation| RunnerResult {
                stdout: "<plan-promise>DONE</plan-promise>\n".to_owned(),
                stderr: String::new(),
                exit_code: 0,
            }),
        ]);

        let (_temp, app) = app(runner);
        let mut delegate = TestDelegate {
            answers: vec![],
            notes: vec![],
            stdout: vec![],
            stderr: vec![],
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
                    stdout: "<plan-promise>CONTINUE</plan-promise>\n".to_owned(),
                    stderr: String::new(),
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
                    stdout: "<plan-promise>DONE</plan-promise>\n".to_owned(),
                    stderr: String::new(),
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
            notes: vec![],
            stdout: vec![],
            stderr: vec![],
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
                let _ = stream.send(RunnerStreamEvent::Stdout("started\n".to_owned()));
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
            notes: vec![],
            stdout: vec![],
            stderr: vec![],
        };

        let error = app
            .run_target_with_control("alpha", control, &mut delegate)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("operation canceled"));
        assert!(
            delegate
                .stdout
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
