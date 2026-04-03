use anyhow::{Context, Result, anyhow};
use camino::{Utf8Path, Utf8PathBuf};
use ralph_core::{CodingAgent, LastRunStatus, RunControl, RunnerConfig, RunnerInvocation};
use ralph_runner::{RunnerAdapter, RunnerStreamEvent};
use tokio::sync::mpsc::unbounded_channel;

use crate::{
    RalphApp, RunDelegate, RunEvent,
    prompt::{CompletionCriterion, ParsedPrompt, interpolate_prompt_env, parse_prompt_directives},
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PreparedPromptRun {
    prompt_path: Utf8PathBuf,
    prompt_name: String,
    target_dir: Utf8PathBuf,
    raw_prompt: Option<String>,
}

impl<R> RalphApp<R>
where
    R: RunnerAdapter,
{
    pub(crate) fn prepare_prompt_run(
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

    pub(crate) fn prepare_inline_prompt_run(
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

    pub(crate) async fn run_prepared_prompt<D>(
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
        let (stream_tx, mut stream_rx) = unbounded_channel();
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

fn resolve_available_agent(preferred: CodingAgent, detected: &[CodingAgent]) -> CodingAgent {
    if detected.is_empty() || detected.contains(&preferred) {
        preferred
    } else {
        detected[0]
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
        RalphApp, RunDelegate, RunEvent,
        prompt::{CompletionCriterion, interpolate_prompt_env, parse_prompt_directives},
    };

    use super::resolve_available_agent;

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
        let config = AppConfig {
            max_iterations: 1,
            ..Default::default()
        };
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
        let config = AppConfig {
            max_iterations: 2,
            ..Default::default()
        };
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
}
