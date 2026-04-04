use std::{
    fs,
    io::Write,
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, anyhow};
use camino::{Utf8Path, Utf8PathBuf};
use ralph_core::{
    AppConfig, LastRunStatus, PlanDrivenPhase, PlanDrivenWorkflowState, WorkflowMode,
};
use ralph_runner::{CommandRunner, InteractiveSessionInvocation};
use tempfile::NamedTempFile;

use crate::{
    RalphApp,
    workflow::{
        PLAN_DRIVEN_GOAL_FILE, PLAN_DRIVEN_PLAN_FILE, PLAN_DRIVEN_SPECS_DIR,
        TASK_DRIVEN_PROGRESS_FILE, WORKFLOW_JOURNAL_FILE, workflow_goal_interview_prompt,
    },
};

const CUSTOM_WORKFLOW_GUIDE: &str = include_str!("../../../docs/custom-workflows.md");

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GoalInterviewOutcome {
    pub goal_changed: bool,
    pub exit_code: Option<i32>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct WorkflowCreatorPaths {
    user_config_root: Utf8PathBuf,
    user_flows_dir: Utf8PathBuf,
    user_prompts_dir: Utf8PathBuf,
    project_dir: Utf8PathBuf,
    project_ralph_dir: Utf8PathBuf,
}

impl<R> RalphApp<R> {
    pub fn rebuild_plan_driven_from_scratch(&self, target: &str) -> Result<()> {
        let mut target_config = self.store.read_target_config(target)?;
        if target_config.mode != Some(WorkflowMode::PlanDriven) {
            return Err(anyhow!(
                "scratch rebuild is only available for plan-driven targets"
            ));
        }

        let target_dir = self.store.target_paths(target)?.dir;
        archive_workflow_artifacts(
            &target_dir,
            &[PLAN_DRIVEN_PLAN_FILE, WORKFLOW_JOURNAL_FILE],
            &[PLAN_DRIVEN_SPECS_DIR],
            "goal_rebuild",
        )?;
        fs::create_dir_all(target_dir.join(PLAN_DRIVEN_SPECS_DIR)).with_context(|| {
            format!(
                "failed to recreate {}",
                target_dir.join(PLAN_DRIVEN_SPECS_DIR)
            )
        })?;

        target_config.workflow = Some(PlanDrivenWorkflowState {
            phase: PlanDrivenPhase::Plan,
            ..PlanDrivenWorkflowState::default()
        });
        target_config.runtime = Some(ralph_core::FlowRuntimeState::default());
        target_config.inflight = None;
        target_config.last_prompt = None;
        target_config.last_run_status = LastRunStatus::NeverRun;
        self.store.write_target_config(&target_config)
    }

    pub fn rebuild_task_driven_from_scratch(&self, target: &str) -> Result<()> {
        let mut target_config = self.store.read_target_config(target)?;
        if target_config.mode != Some(WorkflowMode::TaskDriven) {
            return Err(anyhow!(
                "scratch rebuild is only available for task-driven targets"
            ));
        }

        let target_dir = self.store.target_paths(target)?.dir;
        archive_workflow_artifacts(
            &target_dir,
            &[TASK_DRIVEN_PROGRESS_FILE, WORKFLOW_JOURNAL_FILE],
            &[],
            "task_rebuild",
        )?;

        target_config.workflow = Some(PlanDrivenWorkflowState {
            phase: PlanDrivenPhase::Plan,
            ..PlanDrivenWorkflowState::default()
        });
        target_config.runtime = Some(ralph_core::FlowRuntimeState::default());
        target_config.inflight = None;
        target_config.last_prompt = None;
        target_config.last_run_status = LastRunStatus::NeverRun;
        self.store.write_target_config(&target_config)
    }
}

impl RalphApp<CommandRunner> {
    pub fn run_workflow_creator(&self) -> Result<()> {
        let paths = self.workflow_creator_paths()?;
        fs::create_dir_all(paths.user_flows_dir.as_std_path())
            .with_context(|| format!("failed to create {}", paths.user_flows_dir))?;
        fs::create_dir_all(paths.user_prompts_dir.as_std_path())
            .with_context(|| format!("failed to create {}", paths.user_prompts_dir))?;

        let mut guide_file =
            NamedTempFile::new().context("failed to create workflow creator guide temp file")?;
        guide_file
            .write_all(render_workflow_creator_guide(&paths).as_bytes())
            .context("failed to write workflow creator guide temp file")?;
        let guide_path = Utf8PathBuf::from_path_buf(guide_file.path().to_path_buf())
            .map_err(|_| anyhow!("workflow creator guide temp path is not valid UTF-8"))?;

        let agent = self.config.selected_agent()?;
        let outcome = self.runner.run_interactive_session(
            &agent.interactive,
            &InteractiveSessionInvocation {
                session_name: "workflow_creator".to_owned(),
                initial_prompt: workflow_creator_prompt(&paths, &guide_path),
                project_dir: self.project_dir.clone(),
                target_dir: paths.user_config_root.clone(),
                goal_path: guide_path,
            },
        )?;

        match outcome.exit_code {
            Some(0) | None => Ok(()),
            Some(code) => Err(anyhow!(
                "workflow creator session exited with status {code}"
            )),
        }
    }

    pub fn run_workflow_goal_interview(&self, target: &str) -> Result<GoalInterviewOutcome> {
        let target_config = self.store.read_target_config(target)?;
        if !target_config.uses_hidden_workflow() {
            return Err(anyhow!(
                "AI goal refinement is only available for workflow targets"
            ));
        }

        let target_dir = self.store.target_paths(target)?.dir;
        let goal_path = target_dir.join(PLAN_DRIVEN_GOAL_FILE);
        let before = self
            .store
            .read_file(&goal_path)
            .with_context(|| format!("missing required goal file {}", goal_path))?;
        let agent = self.config.selected_agent()?;
        let outcome = self.runner.run_interactive_session(
            &agent.interactive,
            &InteractiveSessionInvocation {
                session_name: "workflow_goal_interview".to_owned(),
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

    fn workflow_creator_paths(&self) -> Result<WorkflowCreatorPaths> {
        let user_config_file = AppConfig::user_config_path()?
            .ok_or_else(|| anyhow!("workflow creator requires a user config directory"))?;
        let user_config_root = user_config_file.parent().ok_or_else(|| {
            anyhow!("workflow creator could not resolve the Ralph user config root")
        })?;

        Ok(WorkflowCreatorPaths {
            user_config_root: user_config_root.to_owned(),
            user_flows_dir: user_config_root.join("flows"),
            user_prompts_dir: user_config_root.join("prompts"),
            project_dir: self.project_dir.clone(),
            project_ralph_dir: self.project_dir.join(".ralph"),
        })
    }
}

fn render_workflow_creator_guide(paths: &WorkflowCreatorPaths) -> String {
    format!(
        "# Ralph Workflow Creator Session\n\n\
This guide was exported by `ralph workflow-creator` for the current session.\n\n\
## Local Session Paths\n\n\
- Ralph user config root: `{}`\n\
- Default reusable flow directory: `{}`\n\
- Default reusable prompt directory: `{}`\n\
- Current project directory: `{}`\n\
- Current project Ralph directory: `{}`\n\n\
## Default Authoring Policy\n\n\
- Create reusable workflow graph files under `{}` by default.\n\
- Create reusable prompt files under `{}` by default.\n\
- Use the current project only for inspection, examples, and integration snippets unless the user explicitly asks for project-local assets.\n\n\
---\n\n{}",
        paths.user_config_root,
        paths.user_flows_dir,
        paths.user_prompts_dir,
        paths.project_dir,
        paths.project_ralph_dir,
        paths.user_flows_dir,
        paths.user_prompts_dir,
        CUSTOM_WORKFLOW_GUIDE,
    )
}

fn workflow_creator_prompt(paths: &WorkflowCreatorPaths, guide_path: &Utf8Path) -> String {
    format!(
        "You are Ralph's workflow creation assistant.\n\n\
Read the workflow authoring guide at `{guide_path}` before you design or edit any workflow files.\n\n\
Session paths:\n\
- Ralph user config root: `{user_config_root}`\n\
- Default reusable flow directory: `{user_flows_dir}`\n\
- Default reusable prompt directory: `{user_prompts_dir}`\n\
- Current project directory: `{project_dir}`\n\
- Current project Ralph directory: `{project_ralph_dir}`\n\n\
Operating rules:\n\
1. Mirror the user's language.\n\
2. Ask one focused question at a time, but keep going until the workflow requirements are complete and unambiguous.\n\
3. Use the current project to inspect existing targets, prompts, or conventions when that avoids unnecessary questions.\n\
4. By default, create the reusable workflow under `{user_config_root}` so every project can reuse it.\n\
5. Put flow TOML files under `{user_flows_dir}` and prompt files under `{user_prompts_dir}`. You may create nested subdirectories when useful.\n\
6. Do not write project-local workflow assets unless the user explicitly asks for project scope.\n\
7. Once requirements are clear, create the workflow files yourself instead of only describing them.\n\
8. If it helps adoption, also prepare a target entrypoint snippet that projects can add to reference the new user workflow.\n\
9. Finish by summarizing the files you created, where they live, and how a target should point to them.\n\n\
The guide file is the source of truth for the supported user-facing workflow format. Use it as your reference while you interview the user and build the workflow.",
        guide_path = guide_path,
        user_config_root = paths.user_config_root,
        user_flows_dir = paths.user_flows_dir,
        user_prompts_dir = paths.user_prompts_dir,
        project_dir = paths.project_dir,
        project_ralph_dir = paths.project_ralph_dir,
    )
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
    use camino::{Utf8Path, Utf8PathBuf};
    use ralph_core::{AppConfig, PlanDrivenPhase, ScaffoldId};
    use ralph_runner::CommandRunner;

    use crate::RalphApp;

    use super::{WorkflowCreatorPaths, render_workflow_creator_guide, workflow_creator_prompt};

    #[test]
    fn rebuild_plan_driven_from_scratch_archives_plan_artifacts() -> Result<()> {
        let temp = tempfile::tempdir().unwrap();
        let project_dir = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();
        let app = RalphApp::new(project_dir.clone(), AppConfig::default(), CommandRunner);
        app.create_target("demo", Some(ScaffoldId::PlanDriven))?;

        let target_dir = project_dir.join(".ralph/targets/demo");
        std::fs::write(target_dir.join("plan.toml"), "version = 1\n")?;
        std::fs::write(target_dir.join("journal.txt"), "notes\n")?;
        std::fs::create_dir_all(target_dir.join("specs/nested"))?;
        std::fs::write(target_dir.join("specs/nested/api.md"), "# API\n")?;

        app.rebuild_plan_driven_from_scratch("demo")?;

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
            Some(PlanDrivenPhase::Plan)
        );
        Ok(())
    }

    #[test]
    fn rebuild_task_driven_from_scratch_archives_progress_artifacts() -> Result<()> {
        let temp = tempfile::tempdir().unwrap();
        let project_dir = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();
        let app = RalphApp::new(project_dir.clone(), AppConfig::default(), CommandRunner);
        app.create_target("demo", Some(ScaffoldId::TaskDriven))?;

        let target_dir = project_dir.join(".ralph/targets/demo");
        std::fs::write(target_dir.join("progress.toml"), "version = 1\n")?;
        std::fs::write(target_dir.join("journal.txt"), "notes\n")?;

        app.rebuild_task_driven_from_scratch("demo")?;

        assert!(!target_dir.join("progress.toml").exists());
        assert!(!target_dir.join("journal.txt").exists());
        assert!(target_dir.join(".history").exists());
        assert_eq!(
            app.store
                .read_target_config("demo")?
                .workflow
                .as_ref()
                .map(|workflow| workflow.phase),
            Some(PlanDrivenPhase::Plan)
        );
        Ok(())
    }

    #[test]
    fn workflow_creator_guide_mentions_user_scoped_paths() {
        let paths = WorkflowCreatorPaths {
            user_config_root: "/tmp/.config/ralph".into(),
            user_flows_dir: "/tmp/.config/ralph/flows".into(),
            user_prompts_dir: "/tmp/.config/ralph/prompts".into(),
            project_dir: "/tmp/project".into(),
            project_ralph_dir: "/tmp/project/.ralph".into(),
        };

        let rendered = render_workflow_creator_guide(&paths);

        assert!(rendered.contains("/tmp/.config/ralph/flows"));
        assert!(rendered.contains("/tmp/.config/ralph/prompts"));
        assert!(rendered.contains("/tmp/project/.ralph"));
        assert!(rendered.contains("Custom Workflow Authoring"));
    }

    #[test]
    fn workflow_creator_prompt_defaults_to_user_config_scope() {
        let paths = WorkflowCreatorPaths {
            user_config_root: "/tmp/.config/ralph".into(),
            user_flows_dir: "/tmp/.config/ralph/flows".into(),
            user_prompts_dir: "/tmp/.config/ralph/prompts".into(),
            project_dir: "/tmp/project".into(),
            project_ralph_dir: "/tmp/project/.ralph".into(),
        };

        let rendered =
            workflow_creator_prompt(&paths, Utf8Path::new("/tmp/workflow-creator-guide.md"));

        assert!(rendered.contains("/tmp/workflow-creator-guide.md"));
        assert!(
            rendered
                .contains("By default, create the reusable workflow under `/tmp/.config/ralph`")
        );
        assert!(rendered.contains("Do not write project-local workflow assets unless the user explicitly asks for project scope."));
    }
}
