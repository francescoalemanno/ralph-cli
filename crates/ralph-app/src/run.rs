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
            Some(WorkflowMode::PlanDriven) => {
                return self
                    .run_plan_driven_target_with_control(
                        target,
                        prompt_name,
                        target_config,
                        control,
                        delegate,
                        crate::workflow_run::WorkflowRunMode::Smart,
                    )
                    .await;
            }
            Some(WorkflowMode::TaskDriven) => {
                return self
                    .run_task_driven_target_with_control(
                        target,
                        prompt_name,
                        target_config,
                        control,
                        delegate,
                        crate::workflow_run::WorkflowRunMode::Smart,
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
        AppConfig, PlanDrivenPhase, RunControl, RunnerInvocation, RunnerResult, ScaffoldId,
    };
    use ralph_runner::{RunnerAdapter, RunnerStreamEvent};
    use tokio::sync::mpsc::UnboundedSender;

    use crate::{
        RalphApp, RunDelegate, RunEvent,
        workflow::{
            PLAN_DRIVEN_BUILD_PROMPT, PLAN_DRIVEN_PAUSED_PROMPT, PLAN_DRIVEN_PLAN_PROMPT,
            TASK_DRIVEN_BUILD_PROMPT, TASK_DRIVEN_PAUSED_PROMPT, TASK_DRIVEN_REBASE_PROMPT,
        },
    };

    #[derive(Clone)]
    struct ScriptedRunner {
        output: String,
        exit_code: i32,
    }

    #[derive(Clone)]
    struct PlanDrivenRunner {
        seen_prompt_names: Arc<Mutex<Vec<String>>>,
    }

    #[derive(Clone)]
    struct TaskDrivenRunner {
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
    impl RunnerAdapter for PlanDrivenRunner {
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
                PLAN_DRIVEN_PLAN_PROMPT => {
                    "version = 1\n\n[[items]]\ncategory = \"functional\"\ndescription = \"Ship the feature\"\nsteps = [\"Implement it\", \"Verify it\"]\ncompleted = false\n".to_owned()
                }
                PLAN_DRIVEN_BUILD_PROMPT => {
                    "version = 1\n\n[[items]]\ncategory = \"functional\"\ndescription = \"Ship the feature\"\nsteps = [\"Implement it\", \"Verify it\"]\ncompleted = true\n".to_owned()
                }
                other => panic!("unexpected plan-driven prompt {other}"),
            };
            std::fs::write(plan_path, contents).unwrap();

            Ok(RunnerResult {
                output: String::new(),
                exit_code: 0,
            })
        }
    }

    #[async_trait]
    impl RunnerAdapter for TaskDrivenRunner {
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
                TASK_DRIVEN_REBASE_PROMPT => {
                    "version = 1\n\n[[items]]\ndescription = \"Ship the feature\"\nsteps = [\"Implement it\", \"Verify it\"]\ncompleted = false\n".to_owned()
                }
                TASK_DRIVEN_BUILD_PROMPT => {
                    "version = 1\n\n[[items]]\ndescription = \"Ship the feature\"\nsteps = [\"Implement it\", \"Verify it\"]\ncompleted = true\n".to_owned()
                }
                other => panic!("unexpected task-driven prompt {other}"),
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
    async fn plan_driven_targets_plan_then_build_and_keep_building_even_if_goal_changes() {
        let temp = tempfile::tempdir().unwrap();
        let project_dir = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();
        let seen_prompt_names = Arc::new(Mutex::new(Vec::new()));
        let app = RalphApp::new(
            project_dir.clone(),
            AppConfig::default(),
            PlanDrivenRunner {
                seen_prompt_names: seen_prompt_names.clone(),
            },
        );
        app.create_target("demo", Some(ScaffoldId::PlanDriven))
            .unwrap();

        let mut delegate = TestDelegate;
        let summary = app.run_target("demo", None, &mut delegate).await.unwrap();
        assert_eq!(
            summary.last_prompt.as_deref(),
            Some(PLAN_DRIVEN_PLAN_PROMPT)
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
            Some(PlanDrivenPhase::Build)
        );

        let summary = app.run_target("demo", None, &mut delegate).await.unwrap();
        assert_eq!(
            summary.last_prompt.as_deref(),
            Some(PLAN_DRIVEN_BUILD_PROMPT)
        );
        assert_eq!(
            app.store
                .read_target_config("demo")
                .unwrap()
                .workflow
                .as_ref()
                .map(|workflow| workflow.phase),
            Some(PlanDrivenPhase::Paused)
        );

        let summary = app.run_target("demo", None, &mut delegate).await.unwrap();
        assert_eq!(
            summary.last_prompt.as_deref(),
            Some(PLAN_DRIVEN_BUILD_PROMPT)
        );
        assert_eq!(
            *seen_prompt_names.lock().unwrap(),
            vec![
                PLAN_DRIVEN_PLAN_PROMPT.to_owned(),
                PLAN_DRIVEN_PLAN_PROMPT.to_owned(),
                PLAN_DRIVEN_BUILD_PROMPT.to_owned(),
                PLAN_DRIVEN_BUILD_PROMPT.to_owned()
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
            Some(PLAN_DRIVEN_PAUSED_PROMPT)
        );
        assert_eq!(
            *seen_prompt_names.lock().unwrap(),
            vec![
                PLAN_DRIVEN_PLAN_PROMPT.to_owned(),
                PLAN_DRIVEN_PLAN_PROMPT.to_owned(),
                PLAN_DRIVEN_BUILD_PROMPT.to_owned(),
                PLAN_DRIVEN_BUILD_PROMPT.to_owned()
            ]
        );
    }

    #[tokio::test]
    async fn task_driven_targets_rebase_then_build_then_require_choice_on_goal_change() {
        let temp = tempfile::tempdir().unwrap();
        let project_dir = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();
        let seen_prompt_names = Arc::new(Mutex::new(Vec::new()));
        let app = RalphApp::new(
            project_dir.clone(),
            AppConfig::default(),
            TaskDrivenRunner {
                seen_prompt_names: seen_prompt_names.clone(),
            },
        );
        app.create_target("demo", Some(ScaffoldId::TaskDriven))
            .unwrap();

        let mut delegate = TestDelegate;
        let summary = app.run_target("demo", None, &mut delegate).await.unwrap();
        assert_eq!(
            summary.last_prompt.as_deref(),
            Some(TASK_DRIVEN_REBASE_PROMPT)
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
            Some(PlanDrivenPhase::Build)
        );

        let summary = app.run_target("demo", None, &mut delegate).await.unwrap();
        assert_eq!(
            summary.last_prompt.as_deref(),
            Some(TASK_DRIVEN_BUILD_PROMPT)
        );
        assert_eq!(
            *seen_prompt_names.lock().unwrap(),
            vec![
                TASK_DRIVEN_REBASE_PROMPT.to_owned(),
                TASK_DRIVEN_REBASE_PROMPT.to_owned(),
                TASK_DRIVEN_BUILD_PROMPT.to_owned()
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
            Some(TASK_DRIVEN_PAUSED_PROMPT)
        );
        assert_eq!(
            *seen_prompt_names.lock().unwrap(),
            vec![
                TASK_DRIVEN_REBASE_PROMPT.to_owned(),
                TASK_DRIVEN_REBASE_PROMPT.to_owned(),
                TASK_DRIVEN_BUILD_PROMPT.to_owned(),
            ]
        );
    }

    #[tokio::test]
    async fn task_driven_failures_persist_last_run_status() {
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
        app.create_target("demo", Some(ScaffoldId::TaskDriven))
            .unwrap();

        let mut delegate = TestDelegate;
        let error = app
            .run_target("demo", None, &mut delegate)
            .await
            .unwrap_err();

        assert!(error.to_string().contains("runner exited with code 1"));
        let config = app.store.read_target_config("demo").unwrap();
        assert_eq!(
            config.last_prompt.as_deref(),
            Some(TASK_DRIVEN_REBASE_PROMPT)
        );
        assert_eq!(config.last_run_status, ralph_core::LastRunStatus::Failed);
    }
}
