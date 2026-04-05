use anyhow::{Context, Result, anyhow};
use camino::{Utf8Path, Utf8PathBuf};
use ralph_core::{LastRunStatus, RunControl, RunnerConfig, RunnerInvocation};
use ralph_runner::{RunnerAdapter, RunnerStreamEvent};
use tokio::sync::mpsc::unbounded_channel;

use crate::{RalphApp, RunDelegate, RunEvent, prompt::interpolate_prompt_env};

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

    fn parse_prompt_run(&self, prepared: &PreparedPromptRun) -> Result<String> {
        let raw_prompt = match &prepared.raw_prompt {
            Some(raw_prompt) => raw_prompt.clone(),
            None => self
                .store
                .read_file(&prepared.prompt_path)
                .with_context(|| format!("failed to read prompt file {}", prepared.prompt_path))?,
        };
        interpolate_prompt_env(&raw_prompt, &self.project_dir, &prepared.target_dir)
    }

    pub(crate) async fn run_prepared_prompt<D>(
        &self,
        prepared: &PreparedPromptRun,
        max_iterations: usize,
        control: &RunControl,
        delegate: &mut D,
        _completed_summary: &str,
        max_iterations_summary: &str,
    ) -> Result<LastRunStatus>
    where
        D: RunDelegate,
    {
        if max_iterations == 0 {
            return Err(anyhow!("max_iterations must be greater than zero"));
        }

        for iteration in 1..=max_iterations {
            let prompt_text = self.parse_prompt_run(prepared)?;

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

            let config = self.runner_config_for(control)?;
            let result = self
                .execute_runner(
                    &config,
                    RunnerInvocation {
                        prompt_text,
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
        }

        delegate
            .on_event(RunEvent::Finished {
                status: LastRunStatus::MaxIterations,
                summary: max_iterations_summary.to_owned(),
            })
            .await?;
        Ok(LastRunStatus::MaxIterations)
    }

    fn runner_config_for(&self, control: &RunControl) -> Result<RunnerConfig> {
        let agent_id = control
            .agent_id()
            .unwrap_or_else(|| self.config.agent_id().to_owned());
        let agent = self
            .config
            .agent_definition(&agent_id)
            .ok_or_else(|| anyhow!("agent '{}' is not defined", agent_id))?;
        Ok(agent.non_interactive.clone())
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

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use anyhow::Result;
    use async_trait::async_trait;
    use camino::Utf8PathBuf;
    use ralph_core::{
        AppConfig, RunControl, RunnerConfig, RunnerInvocation, RunnerResult, ScaffoldId,
    };
    use ralph_runner::{RunnerAdapter, RunnerStreamEvent};
    use tokio::sync::mpsc::UnboundedSender;

    use crate::{RalphApp, RunDelegate, RunEvent, prompt::interpolate_prompt_env};
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
    struct ConfigCapturingRunner {
        seen_configs: Arc<Mutex<Vec<RunnerConfig>>>,
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
    impl RunnerAdapter for ConfigCapturingRunner {
        async fn run(
            &self,
            config: &ralph_core::RunnerConfig,
            _invocation: RunnerInvocation,
            _control: &RunControl,
            _stream: Option<UnboundedSender<RunnerStreamEvent>>,
        ) -> Result<RunnerResult> {
            self.seen_configs.lock().unwrap().push(config.clone());
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
    async fn single_prompt_targets_run_to_max_iterations() {
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
                "# Prompt\n\nFirst version\n".to_owned(),
                "# Prompt\n\nSecond version\n".to_owned()
            ]
        );
    }

    #[test]
    fn ralph_env_project_and_target_dirs_are_interpolated_to_absolute_unix_paths() {
        let project_dir = Utf8PathBuf::from("/tmp/project");
        let target_dir = Utf8PathBuf::from("/tmp/project/.ralph/targets/demo");
        let interpolated = interpolate_prompt_env(
            "Project {ralph-env:PROJECT_DIR}\nTarget {ralph-env:TARGET_DIR}/progress.txt",
            &project_dir,
            &target_dir,
        )
        .unwrap();

        assert_eq!(
            interpolated,
            "Project /tmp/project\nTarget /tmp/project/.ralph/targets/demo/progress.txt"
        );
    }

    #[tokio::test]
    async fn runtime_agent_override_uses_selected_agent_definition() {
        let temp = tempfile::tempdir().unwrap();
        let project_dir = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();
        let seen_configs = Arc::new(Mutex::new(Vec::new()));
        let app = RalphApp::new(
            project_dir.clone(),
            AppConfig {
                max_iterations: 1,
                ..Default::default()
            },
            ConfigCapturingRunner {
                seen_configs: seen_configs.clone(),
            },
        );
        app.create_target("demo", Some(ScaffoldId::SinglePrompt))
            .unwrap();
        std::fs::write(
            project_dir.join(".ralph/targets/demo/progress.txt"),
            "steady\n",
        )
        .unwrap();

        let mut delegate = TestDelegate;
        let control = RunControl::new();
        control.set_agent_id("raijin");
        let summary = app
            .run_target_with_control("demo", None, control, &mut delegate)
            .await
            .unwrap();

        assert_eq!(
            summary.last_run_status,
            ralph_core::LastRunStatus::MaxIterations
        );
        let seen = seen_configs.lock().unwrap();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].program.as_deref(), Some("raijin"));
    }
}
