use anyhow::{Result, anyhow};
use camino::Utf8Path;
use ralph_core::{LastRunStatus, RunControl, TargetSummary, WorkflowMode};
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
    use std::sync::{Arc, Mutex};

    use anyhow::Result;
    use async_trait::async_trait;
    use camino::Utf8PathBuf;
    use ralph_core::{
        AppConfig, GoalDrivenPhase, RunControl, RunnerInvocation, RunnerResult, ScaffoldId,
    };
    use ralph_runner::{RunnerAdapter, RunnerStreamEvent};
    use tokio::sync::mpsc::UnboundedSender;

    use crate::{
        RalphApp, RunDelegate, RunEvent,
        workflow::{
            GOAL_DRIVEN_BUILD_PROMPT, GOAL_DRIVEN_PAUSED_PROMPT, GOAL_DRIVEN_PLAN_PROMPT,
            TASK_BASED_BUILD_PROMPT, TASK_BASED_PAUSED_PROMPT,
        },
    };

    #[derive(Clone)]
    struct ScriptedRunner {
        output: String,
        exit_code: i32,
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
                GOAL_DRIVEN_PLAN_PROMPT => {
                    "version = 1\n\n[[items]]\ncategory = \"functional\"\ndescription = \"Ship the feature\"\nsteps = [\"Implement it\", \"Verify it\"]\ncompleted = false\n".to_owned()
                }
                GOAL_DRIVEN_BUILD_PROMPT => {
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
                TASK_BASED_BUILD_PROMPT => {
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
            Some(GOAL_DRIVEN_PLAN_PROMPT)
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
            Some(GOAL_DRIVEN_BUILD_PROMPT)
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
            Some(GOAL_DRIVEN_PAUSED_PROMPT)
        );
        assert_eq!(
            *seen_prompt_names.lock().unwrap(),
            vec![
                GOAL_DRIVEN_PLAN_PROMPT.to_owned(),
                GOAL_DRIVEN_PLAN_PROMPT.to_owned(),
                GOAL_DRIVEN_BUILD_PROMPT.to_owned()
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
            Some(GOAL_DRIVEN_PLAN_PROMPT)
        );
        assert_eq!(
            *seen_prompt_names.lock().unwrap(),
            vec![
                GOAL_DRIVEN_PLAN_PROMPT.to_owned(),
                GOAL_DRIVEN_PLAN_PROMPT.to_owned(),
                GOAL_DRIVEN_BUILD_PROMPT.to_owned(),
                GOAL_DRIVEN_PLAN_PROMPT.to_owned(),
                GOAL_DRIVEN_PLAN_PROMPT.to_owned()
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
            Some(TASK_BASED_BUILD_PROMPT)
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
            Some(TASK_BASED_PAUSED_PROMPT)
        );
        assert_eq!(
            *seen_prompt_names.lock().unwrap(),
            vec![TASK_BASED_BUILD_PROMPT.to_owned()]
        );

        std::fs::write(
            project_dir.join(".ralph/targets/demo/GOAL.md"),
            "# Goal\n\nChanged\n",
        )
        .unwrap();
        let summary = app.run_target("demo", None, &mut delegate).await.unwrap();
        assert_eq!(
            summary.last_prompt.as_deref(),
            Some(TASK_BASED_BUILD_PROMPT)
        );
        assert_eq!(
            *seen_prompt_names.lock().unwrap(),
            vec![
                TASK_BASED_BUILD_PROMPT.to_owned(),
                TASK_BASED_BUILD_PROMPT.to_owned()
            ]
        );
    }

    #[tokio::test]
    async fn task_based_failures_persist_last_run_status() {
        let temp = tempfile::tempdir().unwrap();
        let project_dir = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();
        let app = RalphApp::new(
            project_dir.clone(),
            AppConfig::default(),
            ScriptedRunner {
                output: "runner failed".to_owned(),
                exit_code: 1,
            },
        );
        app.create_target("demo", Some(ScaffoldId::TaskBased))
            .unwrap();

        let mut delegate = TestDelegate;
        let error = app
            .run_target("demo", None, &mut delegate)
            .await
            .unwrap_err();

        assert!(error.to_string().contains("runner exited with code 1"));
        let config = app.store.read_target_config("demo").unwrap();
        assert_eq!(config.last_prompt.as_deref(), Some(TASK_BASED_BUILD_PROMPT));
        assert_eq!(config.last_run_status, ralph_core::LastRunStatus::Failed);
    }
}
