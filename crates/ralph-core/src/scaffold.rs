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
                &single_prompt_template(),
            )?;
        }
    }

    Ok(())
}

pub fn bare_prompt_template(scaffold: ScaffoldId) -> String {
    match scaffold {
        ScaffoldId::PlanBuild => plan_build_plan_prompt(),
        ScaffoldId::SinglePrompt => single_prompt_template(),
    }
}

fn write_scaffold_file(target_dir: &Utf8Path, name: &str, contents: &str) -> Result<()> {
    let path = target_dir.join(name);
    atomic_write(&path, contents).with_context(|| format!("failed to write {path}"))
}

fn plan_build_plan_prompt() -> String {
    r#"# Execution policy
1. Study `specs/*`.
2. Study `IMPLEMENTATION_PLAN.md` if present in the repository root.
3. Study the codebase areas that appear to hold shared utilities, core modules, or reusable components.
4. Study the existing source code before deciding something is missing.
5. Decide whether `specs/*`, `IMPLEMENTATION_PLAN.md`, and the existing source code are fully aligned and implementation-ready.
   - If they are fully aligned, continue to 6.
   - If you found gaps, inconsistencies, ambiguities, missing specifications, or plan defects that still need repair, continue to 7.
6. Find the single most high-leverage remaining build item in `IMPLEMENTATION_PLAN.md`.
   - If no build item remains, you MUST run `$RALPH_BIN emit loop-stop:ok no-build-items-remaining`, and then goto 10. Stop.
   - Else run `$RALPH_BIN emit loop-route 1_build.md` and goto 10. Stop.
7. Reconcile `specs/*`, `IMPLEMENTATION_PLAN.md`, and the existing source code. Prefer shared, consolidated solutions already present in the codebase over ad hoc duplication.
8. Update `specs/*` conservatively until a builder could implement without guessing.
9. Update `IMPLEMENTATION_PLAN.md` in the repository root as a prioritized list of remaining work, where each bullet is one concrete, observable outcome small enough for one build loop to finish completely. Then run `$RALPH_BIN emit loop-continue iterating-on-plan`.
10. Stop.

# Constraints
0. NEVER implement, plan ONLY.
1. NEVER assume that something is not-implemented; study the source code.
2. YAGNI ruthlessly - no speculative features.
3. Prefer refining `specs/*` over pushing guesses into implementation.
4. Order `IMPLEMENTATION_PLAN.md` so earlier work unlocks later work and front-loads risk reduction, shared interfaces, migrations, and compatibility work.
5. Fold low-value chores into the item they validate; do not create standalone busywork items unless they materially unblock later work.
6. Prefer vertical-slice plan items over horizontal phase-based items whenever possible, because vertical slices reduce integration risk, surface hidden dependencies earlier, and leave the repository in a more verifiable working state after each build loop.
7. Consider missing elements and plan accordingly. If an element is missing, search first to confirm it does not already exist, then, if needed, author the specification at `specs/FILENAME.md`.

# ULTIMATE GOAL
[project-specific goal]
"#
    .to_owned()
}

fn plan_build_build_prompt() -> String {
    r#"# Execution policy
1. Study `specs/*`.
2. Study `IMPLEMENTATION_PLAN.md` if present in the repository root.
3. Study the existing source code before deciding something is missing.
4. Check whether `IMPLEMENTATION_PLAN.md` and `specs/*` are sufficient to implement without guessing.
   - If they are missing, stale, ambiguous, or materially inconsistent with the code, run `$RALPH_BIN emit loop-route 0_plan.md` and goto 9. Stop.
5. Find the single most high-leverage open item in `IMPLEMENTATION_PLAN.md`.
   - If no open item remains, run `$RALPH_BIN emit loop-route 0_plan.md` and goto 9. Stop.
   - Else run `$RALPH_BIN emit loop-continue single-build-task-taken` and proceed.
6. Execute ONLY the chosen high-priority item completely against the specifications. Do not leave placeholders or partial implementations behind.
7. Run the checks relevant to the code you changed.
8. Update `IMPLEMENTATION_PLAN.md` with completed work and new findings such that the next worker can continue seamlessly. Update `AGENTS.md` only when you learn durable operational guidance about running or debugging the project.
9. Stop.

# Constraints
1. NEVER assume that something is not-implemented; study the source code.
2. YAGNI ruthlessly - no speculative features.
3. If the work requires materially reshaping specs or the plan, route back to `0_plan.md` instead of guessing.
4. Execute only one plan item per run.
"#
    .to_owned()
}

fn single_prompt_template() -> String {
    "# Execution policy\n1. Study `{ralph-env:TARGET_DIR}/progress.txt`.\n2. Study the existing source code.\n4. Find the single most high-leverage item in the requests.\n   - If no request item remains, run `$RALPH_BIN emit loop-stop:ok no-requests-remaining`, goto 7. Stop.\n   - Else run `$RALPH_BIN emit loop-continue single-task-taken` and proceed.\n5. Execute ONLY the chosen high-priority item.\n6. Update `{ralph-env:TARGET_DIR}/progress.txt` with completed work and new findings such that the next worker can continue seamlessly.\n7. Stop.\n\n# Constraints\n1. NEVER assume that something is not-implemented, study the source code.\n2. YAGNI ruthlessly - no speculative features\n\n# REQUESTS\n[item list or file path with plan decomposed in tasks]\n"
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::{bare_prompt_template, materialize_target_scaffold};
    use crate::ScaffoldId;
    use camino::Utf8Path;

    #[test]
    fn bare_prompt_template_returns_markdown() {
        assert!(bare_prompt_template(ScaffoldId::SinglePrompt).contains("# REQUESTS"));
        assert!(bare_prompt_template(ScaffoldId::PlanBuild).contains("# ULTIMATE GOAL"));
        assert!(
            bare_prompt_template(ScaffoldId::PlanBuild)
                .contains("$RALPH_BIN emit loop-route 1_build.md")
        );
        assert!(
            bare_prompt_template(ScaffoldId::SinglePrompt)
                .contains("$RALPH_BIN emit loop-stop:ok no-requests-remaining")
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
    fn materialize_single_prompt_writes_only_the_prompt_file() {
        let temp = tempfile::tempdir().unwrap();
        let target_dir = Utf8Path::from_path(temp.path()).unwrap();

        materialize_target_scaffold(target_dir, ScaffoldId::SinglePrompt).unwrap();

        let prompt = std::fs::read_to_string(target_dir.join("prompt_main.md")).unwrap();

        assert!(
            prompt.contains("$RALPH_BIN emit loop-stop:ok no-requests-remaining")
        );
        assert!(!target_dir.join("progress.txt").exists());
    }
}
