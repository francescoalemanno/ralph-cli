use anyhow::{Result, anyhow};
use camino::Utf8Path;
use ralph_core::{LastRunStatus, RunControl, TargetSummary};
use ralph_runner::RunnerAdapter;

use crate::{RalphApp, RunDelegate};

impl<R> RalphApp<R>
where
    R: RunnerAdapter,
{
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
        let target_summary = self.store.load_target(target)?;
        let prompt = self.select_prompt(&target_summary, prompt_name)?;
        let prepared = self.prepare_prompt_run(&prompt.path, &target_summary.dir)?;
        let max_iterations = target_config
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
                let _ = self.store.set_last_run(target, &prompt.name, status);
            })?;

        self.store.set_last_run(target, &prompt.name, status)?;
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
}

#[cfg(test)]
mod tests {
    use anyhow::Result;
    use async_trait::async_trait;
    use camino::Utf8PathBuf;
    use ralph_core::{AppConfig, RunControl, RunnerInvocation, RunnerResult, ScaffoldId};
    use ralph_runner::{RunnerAdapter, RunnerStreamEvent};
    use tokio::sync::mpsc::UnboundedSender;

    use crate::{RalphApp, RunDelegate, RunEvent};

    #[derive(Clone)]
    struct ScriptedRunner {
        exit_code: i32,
    }

    #[async_trait]
    impl RunnerAdapter for ScriptedRunner {
        async fn run(
            &self,
            _config: &ralph_core::RunnerConfig,
            _invocation: RunnerInvocation,
            _control: &RunControl,
            _stream: Option<UnboundedSender<RunnerStreamEvent>>,
        ) -> Result<RunnerResult> {
            Ok(RunnerResult {
                output: String::new(),
                exit_code: self.exit_code,
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
    async fn single_prompt_targets_run_without_explicit_prompt() {
        let temp = tempfile::tempdir().unwrap();
        let project_dir = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();
        let app = RalphApp::new(
            project_dir,
            AppConfig {
                max_iterations: 1,
                ..Default::default()
            },
            ScriptedRunner { exit_code: 0 },
        );
        app.create_target("demo", Some(ScaffoldId::SinglePrompt))
            .unwrap();

        let mut delegate = TestDelegate;
        let summary = app.run_target("demo", None, &mut delegate).await.unwrap();

        assert_eq!(summary.last_prompt.as_deref(), Some("prompt_main.md"));
        assert_eq!(
            summary.last_run_status,
            ralph_core::LastRunStatus::MaxIterations
        );
    }

    #[tokio::test]
    async fn multi_prompt_targets_require_explicit_prompt_selection() {
        let temp = tempfile::tempdir().unwrap();
        let project_dir = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();
        let app = RalphApp::new(
            project_dir,
            AppConfig {
                max_iterations: 1,
                ..Default::default()
            },
            ScriptedRunner { exit_code: 0 },
        );
        app.create_target("demo", Some(ScaffoldId::PlanBuild))
            .unwrap();

        let mut delegate = TestDelegate;
        let error = app
            .run_target("demo", None, &mut delegate)
            .await
            .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("has multiple prompt files; choose one with --prompt")
        );
    }

    #[tokio::test]
    async fn multi_prompt_targets_run_selected_prompt() {
        let temp = tempfile::tempdir().unwrap();
        let project_dir = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();
        let app = RalphApp::new(
            project_dir,
            AppConfig {
                max_iterations: 1,
                ..Default::default()
            },
            ScriptedRunner { exit_code: 0 },
        );
        app.create_target("demo", Some(ScaffoldId::PlanBuild))
            .unwrap();

        let mut delegate = TestDelegate;
        let summary = app
            .run_target_with_control("demo", Some("1_build.md"), RunControl::new(), &mut delegate)
            .await
            .unwrap();

        assert_eq!(summary.last_prompt.as_deref(), Some("1_build.md"));
    }
}
