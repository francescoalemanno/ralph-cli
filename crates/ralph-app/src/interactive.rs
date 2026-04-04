use std::{fs, io::Write};

use anyhow::{Context, Result, anyhow};
use camino::{Utf8Path, Utf8PathBuf};
use ralph_core::AppConfig;
use ralph_runner::{CommandRunner, InteractiveSessionInvocation};
use tempfile::NamedTempFile;

use crate::RalphApp;

const CUSTOM_WORKFLOW_GUIDE: &str = include_str!("../../../docs/custom-workflows.md");

#[derive(Debug, Clone, PartialEq, Eq)]
struct WorkflowCreatorPaths {
    user_config_root: Utf8PathBuf,
    user_workflows_dir: Utf8PathBuf,
    project_dir: Utf8PathBuf,
    project_ralph_dir: Utf8PathBuf,
}

impl RalphApp<CommandRunner> {
    pub fn run_workflow_creator(&self) -> Result<()> {
        let paths = self.workflow_creator_paths()?;
        fs::create_dir_all(paths.user_workflows_dir.as_std_path())
            .with_context(|| format!("failed to create {}", paths.user_workflows_dir))?;

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
    fn workflow_creator_paths(&self) -> Result<WorkflowCreatorPaths> {
        let user_config_file = AppConfig::user_config_path()?
            .ok_or_else(|| anyhow!("workflow creator requires a user config directory"))?;
        let user_config_root = user_config_file.parent().ok_or_else(|| {
            anyhow!("workflow creator could not resolve the Ralph user config root")
        })?;

        Ok(WorkflowCreatorPaths {
            user_config_root: user_config_root.to_owned(),
            user_workflows_dir: user_config_root.join("workflows"),
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
- Default reusable workflow bundle directory: `{}`\n\
- Current project directory: `{}`\n\
- Current project Ralph directory: `{}`\n\n\
## Default Authoring Policy\n\n\
- Create reusable workflow bundles under `{}` by default.\n\
- Each reusable workflow should live under its own bundle directory with a `workflow.toml` manifest.\n\
- Use the current project only for inspection, examples, and integration snippets unless the user explicitly asks for project-local assets.\n\n\
---\n\n{}",
        paths.user_config_root,
        paths.user_workflows_dir,
        paths.project_dir,
        paths.project_ralph_dir,
        paths.user_workflows_dir,
        CUSTOM_WORKFLOW_GUIDE,
    )
}

fn workflow_creator_prompt(paths: &WorkflowCreatorPaths, guide_path: &Utf8Path) -> String {
    format!(
        "You are Ralph's workflow creation assistant.\n\n\
Read the workflow authoring guide at `{guide_path}` before you design or edit any workflow files.\n\n\
Session paths:\n\
- Ralph user config root: `{user_config_root}`\n\
- Default reusable workflow bundle directory: `{user_workflows_dir}`\n\
- Current project directory: `{project_dir}`\n\
- Current project Ralph directory: `{project_ralph_dir}`\n\n\
Operating rules:\n\
1. Mirror the user's language.\n\
2. Ask one focused question at a time, but keep going until the workflow requirements are complete and unambiguous.\n\
3. Use the current project to inspect existing targets, prompts, or conventions when that avoids unnecessary questions.\n\
4. By default, create the reusable workflow under `{user_config_root}` so every project can reuse it.\n\
5. Put each reusable workflow under `{user_workflows_dir}/<workflow-id>/` with a `workflow.toml` manifest and any `flows/`, `prompts/`, or `templates/` subdirectories it needs.\n\
6. Do not write project-local workflow assets unless the user explicitly asks for project scope.\n\
7. Once requirements are clear, create the workflow files yourself instead of only describing them.\n\
8. If it helps adoption, also tell the user which template id will appear in `ralph new` and how to create a target from it.\n\
9. Finish by summarizing the files you created, where they live, and how the new workflow is discovered.\n\n\
The guide file is the source of truth for the supported user-facing workflow format. Use it as your reference while you interview the user and build the workflow.",
        guide_path = guide_path,
        user_config_root = paths.user_config_root,
        user_workflows_dir = paths.user_workflows_dir,
        project_dir = paths.project_dir,
        project_ralph_dir = paths.project_ralph_dir,
    )
}

#[cfg(test)]
mod tests {
    use camino::Utf8Path;

    use super::{WorkflowCreatorPaths, render_workflow_creator_guide, workflow_creator_prompt};

    #[test]
    fn workflow_creator_guide_mentions_user_scoped_paths() {
        let paths = WorkflowCreatorPaths {
            user_config_root: "/tmp/.config/ralph".into(),
            user_workflows_dir: "/tmp/.config/ralph/workflows".into(),
            project_dir: "/tmp/project".into(),
            project_ralph_dir: "/tmp/project/.ralph".into(),
        };

        let rendered = render_workflow_creator_guide(&paths);

        assert!(rendered.contains("/tmp/.config/ralph/workflows"));
        assert!(rendered.contains("/tmp/project/.ralph"));
        assert!(rendered.contains("Custom Workflow Authoring"));
    }

    #[test]
    fn workflow_creator_prompt_defaults_to_user_config_scope() {
        let paths = WorkflowCreatorPaths {
            user_config_root: "/tmp/.config/ralph".into(),
            user_workflows_dir: "/tmp/.config/ralph/workflows".into(),
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
