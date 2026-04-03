use std::fs;

use anyhow::{Context, Result};
use camino::Utf8Path;

use crate::{ScaffoldId, atomic_write};

pub(crate) fn materialize_target_scaffold(
    target_dir: &Utf8Path,
    scaffold: ScaffoldId,
) -> Result<()> {
    match scaffold {
        ScaffoldId::PlanBuild => {
            write_scaffold_file(target_dir, "0_plan.md", &plan_build_plan_prompt())?;
            write_scaffold_file(target_dir, "1_build.md", &plan_build_build_prompt())?;
        }
        ScaffoldId::TaskBased => {
            write_scaffold_file(target_dir, "GOAL.md", &goal_driven_goal_template())?;
            write_scaffold_file(
                target_dir,
                "progress.toml",
                &task_based_progress_seed_template(),
            )?;
        }
        ScaffoldId::GoalDriven => {
            write_scaffold_file(target_dir, "GOAL.md", &goal_driven_goal_template())?;
            fs::create_dir_all(target_dir.join("specs"))
                .with_context(|| format!("failed to create {}", target_dir.join("specs")))?;
        }
        ScaffoldId::SinglePrompt => {
            write_scaffold_file(target_dir, "prompt_main.md", &single_prompt_template())?;
        }
    }

    Ok(())
}

pub fn bare_prompt_template(scaffold: ScaffoldId) -> String {
    match scaffold {
        ScaffoldId::PlanBuild => plan_build_plan_prompt(),
        ScaffoldId::TaskBased => goal_driven_goal_template(),
        ScaffoldId::GoalDriven => goal_driven_goal_template(),
        ScaffoldId::SinglePrompt => single_prompt_template(),
    }
}

fn write_scaffold_file(target_dir: &Utf8Path, name: &str, contents: &str) -> Result<()> {
    let path = target_dir.join(name);
    atomic_write(&path, contents).with_context(|| format!("failed to write {path}"))
}

fn plan_build_plan_prompt() -> String {
    r#"0a. Study `specs/*`.
0b. Study `IMPLEMENTATION_PLAN.md` if present in the repository root.
0c. Study the codebase areas that appear to hold shared utilities, core modules, or reusable components.
0d. Study the existing source code before deciding something is missing.

1. Identify missing, incomplete, inconsistent, or unverified work by comparing `specs/*`, `IMPLEMENTATION_PLAN.md`, and the existing source code.
2. Update `IMPLEMENTATION_PLAN.md` in the repository root as a prioritized bullet list of remaining work.
3. If specifications are missing or inconsistent, update `specs/*` conservatively and reflect the resulting work in `IMPLEMENTATION_PLAN.md`.
4. Plan only. Do not implement anything.
5. Prefer shared, consolidated solutions in the codebase over ad hoc duplication.
6. If `IMPLEMENTATION_PLAN.md` is already up to date and sufficient for the next build loop, leave it unchanged.

ULTIMATE GOAL - We want to achieve:
[project-specific goal].

Consider missing elements and plan accordingly. If an element is missing, search first to confirm it does not already exist, then, if needed, author the specification at `specs/FILENAME.md`.

{"ralph":"watch","path":"IMPLEMENTATION_PLAN.md"}
"#
    .to_owned()
}

fn plan_build_build_prompt() -> String {
    r#"0a. Study `specs/*`.
0b. Study `IMPLEMENTATION_PLAN.md` if present in the repository root.
0c. Study the existing source code before deciding something is missing.
1. Choose the highest-priority open item from `IMPLEMENTATION_PLAN.md`.
2. Implement only that highest-priority item completely against the specifications. Do not leave placeholders or partial implementations behind.
3. Run the checks relevant to the code you changed.
4. Update `IMPLEMENTATION_PLAN.md` in the repository root with completed work and new findings.
5. Update `AGENTS.md` only when you learn durable operational guidance about running or debugging the project.
6. If you find no work left to do in `IMPLEMENTATION_PLAN.md` and/or `specs/*`, leave `IMPLEMENTATION_PLAN.md` unchanged.

{"ralph":"watch","path":"IMPLEMENTATION_PLAN.md"}
"#
    .to_owned()
}

fn single_prompt_template() -> String {
    "# Requests (not sorted by priority)\n- A\n- B\n- C\n\n# Execution policy\n1. Read {ralph-env:TARGET_DIR}/progress.txt.\n2. Execute the single most high leverage item in \"Requests\".\n3. If an item was executed, update progress in {ralph-env:TARGET_DIR}/progress.txt with the notions about the executed item; else if no item was left to execute, do not change progress.\n4. Stop\n\n{\"ralph\":\"watch\",\"path\":\"{ralph-env:TARGET_DIR}/progress.txt\"}\n"
        .to_owned()
}

fn goal_driven_goal_template() -> String {
    "# Goal\n\nCapture the desired outcome here.\n\n- Requests\n- Constraints\n- Observations\n- Acceptance notes\n"
        .to_owned()
}

fn task_based_progress_seed_template() -> String {
    r#"version = 1

[[items]]
description = "Planning phase"
steps = [
    "Revise progress.toml into a clear ordered list of concrete tasks derived from your studies",
    "Do not start other items",
    "Stop after updating progress.toml"
]
completed = false
"#
    .to_owned()
}

#[cfg(test)]
mod tests {
    use super::{bare_prompt_template, materialize_target_scaffold};
    use crate::ScaffoldId;

    #[test]
    fn workflow_scaffolds_expose_goal_template_for_bare_prompts() {
        let task_based = bare_prompt_template(ScaffoldId::TaskBased);
        let goal_driven = bare_prompt_template(ScaffoldId::GoalDriven);

        assert!(task_based.starts_with("# Goal"));
        assert_eq!(task_based, goal_driven);
    }

    #[test]
    fn goal_driven_scaffold_creates_goal_file_and_specs_dir() {
        let temp = tempfile::tempdir().unwrap();
        let target_dir = camino::Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();

        materialize_target_scaffold(&target_dir, ScaffoldId::GoalDriven).unwrap();

        assert!(target_dir.join("GOAL.md").is_file());
        assert!(target_dir.join("specs").is_dir());
    }
}
