use std::{
    fs,
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, anyhow};
use ralph_core::{GoalDrivenPhase, GoalDrivenWorkflowState, LastRunStatus, WorkflowMode};
use ralph_runner::{CommandRunner, InteractiveSessionInvocation};

use crate::{
    RalphApp,
    workflow::{
        GOAL_DRIVEN_GOAL_FILE, GOAL_DRIVEN_PLAN_FILE, GOAL_DRIVEN_SPECS_DIR,
        TASK_BASED_PROGRESS_FILE, WORKFLOW_JOURNAL_FILE, workflow_goal_interview_prompt,
    },
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GoalInterviewOutcome {
    pub goal_changed: bool,
    pub exit_code: Option<i32>,
}

impl<R> RalphApp<R> {
    pub fn rebuild_goal_driven_from_scratch(&self, target: &str) -> Result<()> {
        let mut target_config = self.store.read_target_config(target)?;
        if target_config.mode != Some(WorkflowMode::GoalDriven) {
            return Err(anyhow!(
                "scratch rebuild is only available for goal-driven targets"
            ));
        }

        let target_dir = self.store.target_paths(target)?.dir;
        archive_workflow_artifacts(
            &target_dir,
            &[GOAL_DRIVEN_PLAN_FILE, WORKFLOW_JOURNAL_FILE],
            &[GOAL_DRIVEN_SPECS_DIR],
            "goal_rebuild",
        )?;
        fs::create_dir_all(target_dir.join(GOAL_DRIVEN_SPECS_DIR)).with_context(|| {
            format!(
                "failed to recreate {}",
                target_dir.join(GOAL_DRIVEN_SPECS_DIR)
            )
        })?;

        target_config.workflow = Some(GoalDrivenWorkflowState {
            phase: GoalDrivenPhase::Plan,
            ..GoalDrivenWorkflowState::default()
        });
        target_config.inflight = None;
        target_config.last_prompt = None;
        target_config.last_run_status = LastRunStatus::NeverRun;
        self.store.write_target_config(&target_config)
    }

    pub fn rebuild_task_based_from_scratch(&self, target: &str) -> Result<()> {
        let mut target_config = self.store.read_target_config(target)?;
        if target_config.mode != Some(WorkflowMode::TaskBased) {
            return Err(anyhow!(
                "scratch rebuild is only available for task-based targets"
            ));
        }

        let target_dir = self.store.target_paths(target)?.dir;
        archive_workflow_artifacts(
            &target_dir,
            &[TASK_BASED_PROGRESS_FILE, WORKFLOW_JOURNAL_FILE],
            &[],
            "task_rebuild",
        )?;

        target_config.workflow = Some(GoalDrivenWorkflowState {
            phase: GoalDrivenPhase::Plan,
            ..GoalDrivenWorkflowState::default()
        });
        target_config.inflight = None;
        target_config.last_prompt = None;
        target_config.last_run_status = LastRunStatus::NeverRun;
        self.store.write_target_config(&target_config)
    }
}

impl RalphApp<CommandRunner> {
    pub fn run_workflow_goal_interview(&self, target: &str) -> Result<GoalInterviewOutcome> {
        let target_config = self.store.read_target_config(target)?;
        if !target_config.uses_hidden_workflow() {
            return Err(anyhow!(
                "AI goal refinement is only available for workflow targets"
            ));
        }

        let target_dir = self.store.target_paths(target)?.dir;
        let goal_path = target_dir.join(GOAL_DRIVEN_GOAL_FILE);
        let before = self
            .store
            .read_file(&goal_path)
            .with_context(|| format!("missing required goal file {}", goal_path))?;
        let agent = self.config.selected_agent()?;
        let outcome = self.runner.run_interactive_session(
            &agent.interactive,
            &InteractiveSessionInvocation {
                initial_prompt: workflow_goal_interview_prompt(&goal_path),
                project_dir: self.project_dir.clone(),
                target_dir,
                goal_path: goal_path.clone(),
            },
        )?;
        let after = self.store.read_file(&goal_path).unwrap_or_default();

        Ok(GoalInterviewOutcome {
            goal_changed: before != after,
            exit_code: outcome.exit_code,
        })
    }
}

fn archive_workflow_artifacts(
    target_dir: &camino::Utf8Path,
    files: &[&str],
    directories: &[&str],
    prefix: &str,
) -> Result<()> {
    let archive_dir = target_dir
        .join(".history")
        .join(format!("{prefix}-{}", archive_stamp()));
    let mut archived_any = false;

    for file in files {
        let path = target_dir.join(file);
        if path.exists() {
            if !archived_any {
                fs::create_dir_all(&archive_dir)
                    .with_context(|| format!("failed to create {}", archive_dir))?;
                archived_any = true;
            }
            fs::rename(&path, archive_dir.join(file))
                .with_context(|| format!("failed to archive {}", path))?;
        }
    }

    for directory in directories {
        let path = target_dir.join(directory);
        if path.exists() {
            if !archived_any {
                fs::create_dir_all(&archive_dir)
                    .with_context(|| format!("failed to create {}", archive_dir))?;
                archived_any = true;
            }
            fs::rename(&path, archive_dir.join(directory))
                .with_context(|| format!("failed to archive {}", path))?;
        }
    }

    Ok(())
}

fn archive_stamp() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use anyhow::Result;
    use camino::Utf8PathBuf;
    use ralph_core::{AppConfig, GoalDrivenPhase, ScaffoldId};
    use ralph_runner::CommandRunner;

    use crate::RalphApp;

    #[test]
    fn rebuild_goal_driven_from_scratch_archives_plan_artifacts() -> Result<()> {
        let temp = tempfile::tempdir().unwrap();
        let project_dir = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();
        let app = RalphApp::new(project_dir.clone(), AppConfig::default(), CommandRunner);
        app.create_target("demo", Some(ScaffoldId::GoalDriven))?;

        let target_dir = project_dir.join(".ralph/targets/demo");
        std::fs::write(target_dir.join("plan.toml"), "version = 1\n")?;
        std::fs::write(target_dir.join("journal.txt"), "notes\n")?;
        std::fs::create_dir_all(target_dir.join("specs/nested"))?;
        std::fs::write(target_dir.join("specs/nested/api.md"), "# API\n")?;

        app.rebuild_goal_driven_from_scratch("demo")?;

        assert!(!target_dir.join("plan.toml").exists());
        assert!(!target_dir.join("journal.txt").exists());
        assert!(target_dir.join("specs").exists());
        assert!(!target_dir.join("specs/nested/api.md").exists());
        assert!(target_dir.join(".history").exists());
        assert_eq!(
            app.store
                .read_target_config("demo")?
                .workflow
                .as_ref()
                .map(|workflow| workflow.phase),
            Some(GoalDrivenPhase::Plan)
        );
        Ok(())
    }

    #[test]
    fn rebuild_task_based_from_scratch_archives_progress_artifacts() -> Result<()> {
        let temp = tempfile::tempdir().unwrap();
        let project_dir = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();
        let app = RalphApp::new(project_dir.clone(), AppConfig::default(), CommandRunner);
        app.create_target("demo", Some(ScaffoldId::TaskBased))?;

        let target_dir = project_dir.join(".ralph/targets/demo");
        std::fs::write(target_dir.join("progress.toml"), "version = 1\n")?;
        std::fs::write(target_dir.join("journal.txt"), "notes\n")?;

        app.rebuild_task_based_from_scratch("demo")?;

        assert!(!target_dir.join("progress.toml").exists());
        assert!(!target_dir.join("journal.txt").exists());
        assert!(target_dir.join(".history").exists());
        assert_eq!(
            app.store
                .read_target_config("demo")?
                .workflow
                .as_ref()
                .map(|workflow| workflow.phase),
            Some(GoalDrivenPhase::Plan)
        );
        Ok(())
    }
}
