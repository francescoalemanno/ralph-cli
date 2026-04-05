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
        ScaffoldId::SinglePrompt => {
            write_scaffold_file(
                target_dir,
                "prompt_main.md",
                &single_prompt_target_template(),
            )?;
            write_scaffold_file(
                target_dir,
                "progress.txt",
                &single_prompt_progress_template(),
            )?;
        }
    }

    Ok(())
}

pub fn bare_prompt_template(scaffold: ScaffoldId) -> String {
    match scaffold {
        ScaffoldId::PlanBuild => plan_build_plan_prompt(),
        ScaffoldId::SinglePrompt => single_prompt_bare_template(),
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

1. Identify missing, incomplete, inconsistent, or unverified work by comparing `specs/*`, `IMPLEMENTATION_PLAN.md`, and the existing source code. Prefer shared, consolidated solutions in the codebase over ad hoc duplication.
2. If specifications are missing or ambiguous, update `specs/*` conservatively until a builder could implement without guessing. Capture, when relevant:
   a. user-visible outcomes and acceptance checks
   b. explicit scope boundaries and non-goals
   c. interfaces, data flow, storage, and integration points touched
   d. migrations, rollout or backward-compatibility needs, and operational constraints
   e. verification strategy, failure modes, and observability or debugging notes
   f. risks, open questions, and assumptions that must be resolved before coding
3. If unresolved uncertainty would materially change implementation order, architecture, or correctness, keep refining `specs/*` instead of pushing guesses into `IMPLEMENTATION_PLAN.md`.
4. Update `IMPLEMENTATION_PLAN.md` in the repository root as a prioritized bullet list of remaining work.
5. Each bullet must describe one concrete, observable outcome, not a vague activity or component area.
6. Order the bullets so earlier work unlocks later work and front-loads risk reduction, shared interfaces, migrations, and compatibility.
7. Keep each bullet small enough that one build loop can finish the top item completely, including verification, while leaving the repository in a coherent state.
8. Fold low-value chores into the bullet they validate; do not create standalone busywork bullets unless they materially unblock later work.
9. Plan only. Do not implement anything.
10. If `IMPLEMENTATION_PLAN.md` is already up to date and sufficient for the next build loop, leave it unchanged.

ULTIMATE GOAL - We want to achieve:
[project-specific goal].

Consider missing elements and plan accordingly. If an element is missing, search first to confirm it does not already exist, then, if needed, author the specification at `specs/FILENAME.md`.
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
"#
    .to_owned()
}

fn single_prompt_target_template() -> String {
    "# Requests (not sorted by priority)\n- A\n- B\n- C\n\n# Execution policy\n1a. Study the existing source code before deciding something is missing.\n1b. Study `{ralph-env:TARGET_DIR}/progress.txt`.\n2. Execute the single most high leverage remaining item in \"Requests\".\n3. Update `{ralph-env:TARGET_DIR}/progress.txt` with completed work and new findings when that keeps the next loop grounded.\n4. Stop.\n"
        .to_owned()
}

fn single_prompt_bare_template() -> String {
    "# Requests (not sorted by priority)\n- A\n- B\n- C\n\n# Execution policy\n1. Read the existing source code before deciding something is missing.\n2. Execute the single most high leverage item in \"Requests\".\n3. Update this file with completed work and new findings when that keeps the next loop grounded.\n4. Stop.\n"
        .to_owned()
}

fn single_prompt_progress_template() -> String {
    "Completed work:\n- \n\nOpen findings:\n- \n\nNext candidate work:\n- \n".to_owned()
}

#[cfg(test)]
mod tests {
    use super::{bare_prompt_template, materialize_target_scaffold};
    use crate::ScaffoldId;
    use camino::Utf8Path;

    #[test]
    fn bare_prompt_template_returns_markdown() {
        assert!(bare_prompt_template(ScaffoldId::SinglePrompt).contains("# Requests"));
        assert!(bare_prompt_template(ScaffoldId::PlanBuild).contains("ULTIMATE GOAL"));
        assert!(
            bare_prompt_template(ScaffoldId::SinglePrompt)
                .contains("Update this file with completed work")
        );
    }

    #[test]
    fn materialize_plan_build_writes_both_prompt_files() {
        let temp = tempfile::tempdir().unwrap();
        let target_dir = Utf8Path::from_path(temp.path()).unwrap();

        materialize_target_scaffold(target_dir, ScaffoldId::PlanBuild).unwrap();

        assert!(target_dir.join("0_plan.md").exists());
        assert!(target_dir.join("1_build.md").exists());
    }

    #[test]
    fn materialize_single_prompt_writes_progress_sidecar() {
        let temp = tempfile::tempdir().unwrap();
        let target_dir = Utf8Path::from_path(temp.path()).unwrap();

        materialize_target_scaffold(target_dir, ScaffoldId::SinglePrompt).unwrap();

        let prompt = std::fs::read_to_string(target_dir.join("prompt_main.md")).unwrap();
        let progress = std::fs::read_to_string(target_dir.join("progress.txt")).unwrap();

        assert!(
            prompt.contains("Update `{ralph-env:TARGET_DIR}/progress.txt` with completed work")
        );
        assert!(progress.contains("Completed work:"));
        assert!(progress.contains("Open findings:"));
    }
}
