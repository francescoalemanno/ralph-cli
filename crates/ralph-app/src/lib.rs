use std::{env, process::Command};

use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use camino::{Utf8Path, Utf8PathBuf};
use ralph_core::{
    AppConfig, CodingAgent, LastRunStatus, RunControl, RunnerConfig, RunnerInvocation, ScaffoldId,
    TargetReview, TargetStore, TargetSummary,
};
use ralph_runner::{CommandRunner, RunnerAdapter, RunnerStreamEvent};

const WATCH_TAG_PREFIX: &str = "<<ralph-watch:";
const RALPH_ENV_PROJECT_DIR: &str = "{ralph-env:PROJECT_DIR}";
const RALPH_ENV_TARGET_DIR: &str = "{ralph-env:TARGET_DIR}";
const RALPH_ENV_PROMPT_PATH: &str = "{ralph-env:PROMPT_PATH}";
const RALPH_ENV_PROMPT_NAME: &str = "{ralph-env:PROMPT_NAME}";

#[derive(Debug, Clone, PartialEq, Eq)]
struct ParsedPrompt {
    prompt_text: String,
    watched_files: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PreparedPromptRun {
    prompt_path: Utf8PathBuf,
    prompt_name: String,
    target_dir: Utf8PathBuf,
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

    pub fn edit_prompt(&self, target: &str, prompt_name: Option<&str>) -> Result<()> {
        let prompt = self.resolve_prompt(target, prompt_name)?;
        self.open_in_editor(&prompt.path)
    }

    pub fn edit_prompt_file(&self, prompt_path: &Utf8Path) -> Result<()> {
        self.open_in_editor(prompt_path)
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
        };
        self.parse_prompt_run(&prepared)?;
        Ok(prepared)
    }

    fn parse_prompt_run(&self, prepared: &PreparedPromptRun) -> Result<ParsedPrompt> {
        let raw_prompt = self
            .store
            .read_file(&prepared.prompt_path)
            .with_context(|| format!("failed to read prompt file {}", prepared.prompt_path))?;
        let interpolated_prompt = interpolate_prompt_env(
            &raw_prompt,
            &self.project_dir,
            &prepared.target_dir,
            &prepared.prompt_path,
            &prepared.prompt_name,
        )?;
        Ok(parse_prompt_directives(&interpolated_prompt))
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
            let watched_before = self.read_watched_files(&parsed_prompt.watched_files)?;

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

            if !parsed_prompt.watched_files.is_empty()
                && watched_before == self.read_watched_files(&parsed_prompt.watched_files)?
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

    fn read_watched_files(&self, watched_files: &[String]) -> Result<Vec<Option<String>>> {
        watched_files
            .iter()
            .map(|name| {
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
    let mut watched_files = Vec::new();
    let mut cleaned_lines = Vec::new();

    for line in prompt_text.lines() {
        let mut remaining = line;
        let mut cleaned = String::new();

        while let Some(start) = remaining.find(WATCH_TAG_PREFIX) {
            cleaned.push_str(&remaining[..start]);
            let after_prefix = &remaining[start + WATCH_TAG_PREFIX.len()..];
            let Some(end) = after_prefix.find(">>") else {
                cleaned.push_str(&remaining[start..]);
                remaining = "";
                break;
            };

            let watched = after_prefix[..end].trim();
            if !watched.is_empty() && !watched_files.iter().any(|item| item == watched) {
                watched_files.push(watched.to_owned());
            }
            remaining = &after_prefix[end + 2..];
        }

        cleaned.push_str(remaining);
        if !cleaned.trim().is_empty() {
            cleaned_lines.push(cleaned);
        }
    }

    ParsedPrompt {
        prompt_text: cleaned_lines.join("\n"),
        watched_files,
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
        AppConfig, CodingAgent, RunControl, RunnerInvocation, RunnerResult, ScaffoldId,
    };
    use ralph_runner::{RunnerAdapter, RunnerStreamEvent};
    use tokio::sync::mpsc::UnboundedSender;

    use crate::{
        RalphApp, RunDelegate, RunEvent, interpolate_prompt_env, parse_prompt_directives,
        resolve_available_agent,
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
        app.create_target("demo", Some(ScaffoldId::Blank)).unwrap();
        std::fs::write(
            project_dir.join(".ralph/targets/demo/prompt_main.md"),
            "<<ralph-watch:IMPLEMENTATION_PLAN.md>>\n\n# Prompt\n",
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
    async fn blank_targets_still_run_to_max_iterations_without_plan_change_stop() {
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
        app.create_target("demo", Some(ScaffoldId::Blank)).unwrap();
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
        app.create_target("demo", Some(ScaffoldId::Blank)).unwrap();
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
            "<<ralph-watch:IMPLEMENTATION_PLAN.md>>\n# Prompt\nBody\n<<ralph-watch: specs/api.md >>",
        );

        assert_eq!(
            parsed.watched_files,
            vec!["IMPLEMENTATION_PLAN.md", "specs/api.md"]
        );
        assert_eq!(parsed.prompt_text, "# Prompt\nBody");
    }

    #[test]
    fn ralph_env_target_dir_is_interpolated_to_absolute_unix_path() {
        let project_dir = Utf8PathBuf::from("/tmp/project");
        let target_dir = Utf8PathBuf::from("/tmp/project/.ralph/targets/demo");
        let prompt_path = target_dir.join("prompt_main.md");
        let interpolated = interpolate_prompt_env(
            "<<ralph-watch:{ralph-env:TARGET_DIR}/progress.txt>>\nRead {ralph-env:TARGET_DIR}/progress.txt",
            &project_dir,
            &target_dir,
            &prompt_path,
            "prompt_main.md",
        )
        .unwrap();

        assert_eq!(
            interpolated,
            "<<ralph-watch:/tmp/project/.ralph/targets/demo/progress.txt>>\nRead /tmp/project/.ralph/targets/demo/progress.txt"
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
}
