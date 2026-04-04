use std::{
    fs,
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, anyhow};
use camino::{Utf8Path, Utf8PathBuf};
use ralph_core::{PlanDrivenPhase, TargetConfig, TargetStore};
use sha2::{Digest, Sha256};

pub(crate) const PLAN_DRIVEN_GOAL_FILE: &str = "GOAL.md";
pub(crate) const PLAN_DRIVEN_PLAN_FILE: &str = "plan.toml";
pub(crate) const PLAN_DRIVEN_SPECS_DIR: &str = "specs";
pub(crate) const PLAN_DRIVEN_PLAN_PROMPT: &str = "plan_driven_plan";
pub(crate) const PLAN_DRIVEN_BUILD_PROMPT: &str = "plan_driven_build";
pub(crate) const PLAN_DRIVEN_PAUSED_PROMPT: &str = "plan_driven_paused";
pub(crate) const TASK_DRIVEN_PROGRESS_FILE: &str = "progress.toml";
pub(crate) const WORKFLOW_JOURNAL_FILE: &str = "journal.txt";
pub(crate) const TASK_DRIVEN_REBASE_PROMPT: &str = "task_driven_rebase";
pub(crate) const TASK_DRIVEN_BUILD_PROMPT: &str = "task_driven_build";
pub(crate) const TASK_DRIVEN_PAUSED_PROMPT: &str = "task_driven_paused";

const RALPH_ENV_TARGET_DIR: &str = "{ralph-env:TARGET_DIR}";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkflowKind {
    PlanDriven,
    TaskDriven,
}

impl WorkflowKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::PlanDriven => "plan_driven",
            Self::TaskDriven => "task_driven",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkflowDerivedState {
    Missing,
    Fresh,
    Stale,
}

impl WorkflowDerivedState {
    pub fn label(self) -> &'static str {
        match self {
            Self::Missing => "missing",
            Self::Fresh => "fresh",
            Self::Stale => "stale",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkflowRunAdvice {
    Build,
    Rebase,
    Choose,
    NoWork,
}

impl WorkflowRunAdvice {
    pub fn label(self) -> &'static str {
        match self {
            Self::Build => "build",
            Self::Rebase => "rebase",
            Self::Choose => "choose",
            Self::NoWork => "no_work",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkflowAction {
    Build,
    Rebase,
}

impl WorkflowAction {
    pub fn label(self) -> &'static str {
        match self {
            Self::Build => "build",
            Self::Rebase => "rebase",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WorkflowStatus {
    pub kind: WorkflowKind,
    pub derived_state: WorkflowDerivedState,
    pub run_advice: WorkflowRunAdvice,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PlanDrivenAction {
    Plan,
    Build,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TaskDrivenAction {
    Rebase,
    Build,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PlanDrivenHashes {
    pub(crate) goal_hash: String,
    pub(crate) content_hash: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TaskDrivenHashes {
    pub(crate) goal_hash: String,
    pub(crate) content_hash: String,
}

pub(crate) fn select_task_driven_build_needed(
    config: &TargetConfig,
    hashes: &TaskDrivenHashes,
) -> bool {
    if config.inflight.is_some() {
        return true;
    }

    let workflow = config.workflow.clone().unwrap_or_default();
    match workflow.phase {
        PlanDrivenPhase::Paused => {
            workflow.last_content_hash.as_deref() != Some(hashes.content_hash.as_str())
        }
        PlanDrivenPhase::Plan | PlanDrivenPhase::Build => true,
    }
}

pub(crate) fn plan_driven_hashes(
    store: &TargetStore,
    target_dir: &Utf8Path,
) -> Result<PlanDrivenHashes> {
    let goal_path = target_dir.join(PLAN_DRIVEN_GOAL_FILE);
    let goal_contents = store
        .read_file(&goal_path)
        .with_context(|| format!("missing required goal file {}", goal_path))?;
    let goal_hash = hash_bytes(goal_contents.as_bytes());

    let mut hasher = Sha256::new();
    hash_named_contents(&mut hasher, PLAN_DRIVEN_GOAL_FILE, goal_contents.as_bytes());

    let plan_path = target_dir.join(PLAN_DRIVEN_PLAN_FILE);
    if plan_path.exists() {
        hash_named_contents(
            &mut hasher,
            PLAN_DRIVEN_PLAN_FILE,
            store.read_file(&plan_path)?.as_bytes(),
        );
    }

    let specs_dir = target_dir.join(PLAN_DRIVEN_SPECS_DIR);
    if specs_dir.exists() {
        let mut paths = walk_files(&specs_dir)?;
        paths.sort();
        for path in paths {
            let relative = path
                .strip_prefix(target_dir)
                .map_err(|_| anyhow!("failed to compute relative path for {}", path))?;
            hash_named_contents(
                &mut hasher,
                relative.as_str(),
                store.read_file(&path)?.as_bytes(),
            );
        }
    }

    Ok(PlanDrivenHashes {
        goal_hash,
        content_hash: format!("sha256:{:x}", hasher.finalize()),
    })
}

pub(crate) fn task_driven_hashes(
    store: &TargetStore,
    target_dir: &Utf8Path,
) -> Result<TaskDrivenHashes> {
    let goal_path = target_dir.join(PLAN_DRIVEN_GOAL_FILE);
    let goal_contents = store
        .read_file(&goal_path)
        .with_context(|| format!("missing required goal file {}", goal_path))?;
    let goal_hash = hash_bytes(goal_contents.as_bytes());

    let mut hasher = Sha256::new();
    hash_named_contents(&mut hasher, PLAN_DRIVEN_GOAL_FILE, goal_contents.as_bytes());

    let progress_path = target_dir.join(TASK_DRIVEN_PROGRESS_FILE);
    if progress_path.exists() {
        hash_named_contents(
            &mut hasher,
            TASK_DRIVEN_PROGRESS_FILE,
            store.read_file(&progress_path)?.as_bytes(),
        );
    }

    Ok(TaskDrivenHashes {
        goal_hash,
        content_hash: format!("sha256:{:x}", hasher.finalize()),
    })
}

pub(crate) fn plan_driven_workflow_status(
    config: &TargetConfig,
    hashes: &PlanDrivenHashes,
    target_dir: &Utf8Path,
) -> WorkflowStatus {
    if let Some(inflight) = &config.inflight {
        return WorkflowStatus {
            kind: WorkflowKind::PlanDriven,
            derived_state: WorkflowDerivedState::Fresh,
            run_advice: match inflight.phase {
                PlanDrivenPhase::Plan => WorkflowRunAdvice::Rebase,
                PlanDrivenPhase::Build | PlanDrivenPhase::Paused => WorkflowRunAdvice::Build,
            },
        };
    }

    let workflow = config.workflow.clone().unwrap_or_default();
    let has_plan = target_dir.join(PLAN_DRIVEN_PLAN_FILE).exists();
    let derived_state = if !has_plan || workflow.last_planned_at.is_none() {
        WorkflowDerivedState::Missing
    } else if workflow.last_goal_hash.as_deref() != Some(hashes.goal_hash.as_str()) {
        WorkflowDerivedState::Stale
    } else {
        WorkflowDerivedState::Fresh
    };

    let run_advice = match derived_state {
        WorkflowDerivedState::Missing => WorkflowRunAdvice::Rebase,
        WorkflowDerivedState::Stale => WorkflowRunAdvice::Choose,
        WorkflowDerivedState::Fresh => WorkflowRunAdvice::Build,
    };

    WorkflowStatus {
        kind: WorkflowKind::PlanDriven,
        derived_state,
        run_advice,
    }
}

pub(crate) fn task_driven_workflow_status(
    config: &TargetConfig,
    hashes: &TaskDrivenHashes,
    target_dir: &Utf8Path,
) -> WorkflowStatus {
    if let Some(inflight) = &config.inflight {
        return WorkflowStatus {
            kind: WorkflowKind::TaskDriven,
            derived_state: WorkflowDerivedState::Fresh,
            run_advice: match inflight.phase {
                PlanDrivenPhase::Plan => WorkflowRunAdvice::Rebase,
                PlanDrivenPhase::Build | PlanDrivenPhase::Paused => WorkflowRunAdvice::Build,
            },
        };
    }

    let workflow = config.workflow.clone().unwrap_or_default();
    let has_progress = target_dir.join(TASK_DRIVEN_PROGRESS_FILE).exists();
    let derived_state = if !has_progress || workflow.last_goal_hash.is_none() {
        WorkflowDerivedState::Missing
    } else if workflow.last_goal_hash.as_deref() != Some(hashes.goal_hash.as_str()) {
        WorkflowDerivedState::Stale
    } else {
        WorkflowDerivedState::Fresh
    };

    let run_advice = match derived_state {
        WorkflowDerivedState::Missing => WorkflowRunAdvice::Rebase,
        WorkflowDerivedState::Stale => WorkflowRunAdvice::Choose,
        WorkflowDerivedState::Fresh => {
            if select_task_driven_build_needed(config, hashes) {
                WorkflowRunAdvice::Build
            } else {
                WorkflowRunAdvice::NoWork
            }
        }
    };

    WorkflowStatus {
        kind: WorkflowKind::TaskDriven,
        derived_state,
        run_advice,
    }
}

pub(crate) fn plan_driven_plan_prompt() -> String {
    format!(
        r#"1. Study these inputs before planning:
   a. Study `{target_dir}/GOAL.md`.
   b. Study `{target_dir}/plan.toml` if it exists.
   c. Study all spec files in `{target_dir}/specs/`.
   d. Study `{target_dir}/{journal_file}` if it exists.
2. Study the relevant repository documentation and source code (do not assume something is not implemented, look deeply). Prefer extending existing mechanisms over duplicating them.
3. Rebase the planning artifacts to the current goal instead of starting from scratch unless the existing plan/spec context is clearly invalid.
4. Create or revise the spec files in `{target_dir}/specs/` until a builder could implement without guessing. Capture, when relevant:
   a. user-visible outcomes and acceptance checks
   b. explicit scope boundaries and non-goals
   c. interfaces, data flow, storage, and integration points touched
   d. migrations, rollout or backward-compatibility needs, and operational constraints
   e. verification strategy, failure modes, and observability or debugging notes
   f. risks, open questions, and assumptions that must be resolved before coding
5. If uncertainty remains that would materially change architecture, ordering, or correctness, keep refining `{target_dir}/specs/`. Do not push unresolved design decisions into `{target_dir}/plan.toml`.
6. Only after the specifications are coherent and sufficient, create or revise `{target_dir}/plan.toml` as the current operational plan.
7. `plan.toml` must stay valid TOML and follow this exact shape:

```toml
version = 1

[[items]]
category = "functional" # or "non_functional"
description = "Describe one concrete outcome"
steps = ["List the ordered implementation and verification steps"]
completed = false
```

8. Each `items.description` must name one concrete, observable outcome, not a vague activity or component area.
9. Each `items.steps` list must be the ordered implementation and verification sequence for that one outcome.
10. Preserve completed items that are still coherent with the current goal, and keep their ordering unless the goal makes them obsolete.
11. Remove or rewrite incomplete items and specs that are no longer coherent with the goal. Do not preserve stale planning context just because it already exists.
12. Decompose the plan into the smallest high-leverage items that can each be completed in one focused build iteration while leaving the repository in a coherent state.
13. Keep the items in the exact execution order. Earlier items must prepare later items; do not rely on later items to make earlier items possible.
14. Front-load prerequisite and risk-reduction work. Resolve unknowns, shared interfaces, migrations, and compatibility work before dependent feature slices.
15. Use `category` to distinguish functional and non-functional work when relevant.
16. Include non-functional items only when they are required to satisfy the goal or reduce a concrete delivery risk.
17. Fold low-value chores into the item they validate; do not create standalone busywork items unless they materially unblock later work.
18. Keep every incomplete item at `completed = false`.
19. Plan only. Do not implement product code or tests.
20. If the specifications and `plan.toml` are already correct and sufficient, leave `{target_dir}/plan.toml` unchanged.

{{"ralph":"watch","path":"{target_dir}/plan.toml"}}
"#,
        target_dir = RALPH_ENV_TARGET_DIR,
        journal_file = WORKFLOW_JOURNAL_FILE
    )
}

pub(crate) fn plan_driven_build_prompt() -> String {
    format!(
        r#"1. Study these inputs before building:
   a. Study `{target_dir}/GOAL.md`.
   b. Study `{target_dir}/plan.toml`.
   c. Study all spec files in `{target_dir}/specs/`.
   d. Study `{target_dir}/journal.txt` if it exists.
   e. Study `AGENTS.md` if it exists.
2. Study the relevant repository documentation and source code (do not assume something is not implemented, look deeply).
3. Select the single highest-priority open item with the highest leverage from `{target_dir}/plan.toml`.
4. Execute only that item completely against the current target-local specifications and plan. Do not leave placeholders or partial implementations behind.
5. Run the checks relevant to the code you changed.
6. Update `{target_dir}/plan.toml` so it accurately records completed work and any remaining follow-up.
   `plan.toml` must stay valid TOML and follow this exact shape:

```toml
version = 1

[[items]]
category = "functional"
description = "Describe one concrete outcome"
steps = ["List the ordered implementation and verification steps"]
completed = false
```
7. Create or update `{target_dir}/journal.txt` as a free-form builder journal for future iterations. Record what you changed, what you verified, and any concrete follow-up notes useful to the next build iteration.
8. Do not edit `{target_dir}/GOAL.md`.
9. Update `AGENTS.md` only when you learn durable operational guidance about running or debugging the project.

{{"ralph":"complete_when","type":"no_line_contains_all","path":"{target_dir}/plan.toml","tokens":["completed","false"]}}
"#,
        target_dir = RALPH_ENV_TARGET_DIR
    )
}

pub(crate) fn task_driven_rebase_prompt() -> String {
    format!(
        r#"1. Study these inputs before rebasing the task backlog:
   a. Study `{target_dir}/GOAL.md` as the authoritative intent.
   b. Study `{target_dir}/progress.toml` if it exists.
   c. Study `{target_dir}/{journal_file}` if it exists.
   d. Study `AGENTS.md` if it exists.
2. Study the relevant repository documentation and source code (do not assume something is not implemented, look deeply).
3. Rebase `{target_dir}/progress.toml` to match the current goal.
4. Preserve completed items that are still coherent with the goal. Keep their `completed = true` state when they still represent valid delivered work.
5. Rewrite or remove items that are no longer coherent with the goal. Add new items required by the updated goal.
6. Keep the backlog small, concrete, and execution-oriented. Each item should still be completable in one focused build iteration.
7. `progress.toml` must stay valid TOML and follow this exact shape:

```toml
version = 1

[[items]]
description = "..."
steps = ["..."]
completed = false
```
8. Keep the items in execution order. Earlier items must unblock later ones.
9. Backlog rebase only. Do not implement product code or tests in this run.
10. If the existing `progress.toml` is already correct for the current goal, leave it unchanged.

{{"ralph":"watch","path":"{target_dir}/progress.toml"}}
"#,
        target_dir = RALPH_ENV_TARGET_DIR,
        journal_file = WORKFLOW_JOURNAL_FILE
    )
}

pub(crate) fn task_driven_build_prompt() -> String {
    format!(
        r#"1. Study these inputs before building:
   a. Study `{target_dir}/GOAL.md` as the CEO input.
   b. Study `{target_dir}/progress.toml`.
   c. Study `{target_dir}/{journal_file}` if it exists.
   d. Study `AGENTS.md` if it exists.
2. Study the relevant repository documentation and source code (do not assume something is not implemented, look deeply).
3. Select the single highest-priority open item with the highest leverage from `{target_dir}/progress.toml`.
4. Execute only that item completely. Do not leave placeholders or partial implementations behind.
5. Run the checks relevant to the code you changed.
6. Update `{target_dir}/progress.toml` so it accurately records completed work and any remaining follow-up.
   `progress.toml` must stay valid TOML and follow this exact shape:

```toml
version = 1

[[items]]
description = "..."
steps = ["..."]
completed = false
```
7. Create or update `{target_dir}/{journal_file}` as a free-form builder journal for future iterations. Record what you changed, what you verified, and any concrete follow-up notes useful to the next build iteration.
8. Do not edit `{target_dir}/GOAL.md`.
9. Update `AGENTS.md` only when you learn durable operational guidance about running or debugging the project.

{{"ralph":"complete_when","type":"no_line_contains_all","path":"{target_dir}/progress.toml","tokens":["completed","false"]}}
"#,
        target_dir = RALPH_ENV_TARGET_DIR,
        journal_file = WORKFLOW_JOURNAL_FILE
    )
}

pub(crate) fn workflow_goal_interview_prompt(goal_path: &Utf8Path) -> String {
    format!(
        r#"You are helping the user refine the target goal stored in `{goal_path}`.

1. Read `{goal_path}` before doing anything else.
2. Treat the current contents as a draft. Do not assume they are complete or precise.
3. Then run an interactive interview with the user until the desired outcome is concrete and unambiguous.
4. Ask one focused question at a time. For each question:
   a. explain briefly why the question matters
   b. provide your recommended answer
5. If a question can be answered by studying this repository, inspect the repository instead of asking the user.
6. Walk the full decision tree until all material ambiguities are resolved. Cover at least:
   a. desired outcomes and acceptance criteria
   b. scope boundaries and explicit non-goals
   c. user-visible behavior, interfaces, and integration points
   d. technical constraints, migrations, rollout concerns, and compatibility
   e. failure modes, edge cases, observability, and verification
7. Surface contradictions, hidden assumptions, and unresolved dependencies explicitly. Do not pretend agreement exists when it does not.
8. Do not edit `{goal_path}` until the user has explicitly confirmed the final direction.
9. Once agreement is explicit, rewrite `{goal_path}` into a complete self-contained specification that a planner or builder can execute without guessing.
10. The rewritten file must be concrete, implementation-oriented, and precise. Record any deliberate assumptions or deferred choices explicitly instead of leaving them implicit.
"#
    )
}

pub(crate) fn current_unix_timestamp() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

fn hash_bytes(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("sha256:{:x}", hasher.finalize())
}

fn hash_named_contents(hasher: &mut Sha256, name: &str, contents: &[u8]) {
    hasher.update(name.as_bytes());
    hasher.update([0]);
    hasher.update(contents);
    hasher.update([0xff]);
}

fn walk_files(dir: &Utf8Path) -> Result<Vec<Utf8PathBuf>> {
    let mut files = Vec::new();
    for entry in fs::read_dir(dir).with_context(|| format!("failed to read {}", dir))? {
        let entry = entry?;
        let path = Utf8PathBuf::from_path_buf(entry.path())
            .map_err(|_| anyhow!("non-UTF8 path under {}", dir))?;
        if path.is_dir() {
            files.extend(walk_files(&path)?);
        } else if path.is_file() {
            files.push(path);
        }
    }
    Ok(files)
}

#[cfg(test)]
mod tests {
    use crate::workflow::{WorkflowDerivedState, WorkflowRunAdvice};
    use camino::{Utf8Path, Utf8PathBuf};
    use ralph_core::TargetStore;
    use ralph_core::{
        LastRunStatus, PlanDrivenPhase, PlanDrivenWorkflowState, TargetConfig, WorkflowMode,
    };

    use super::{plan_driven_hashes, plan_driven_workflow_status, workflow_goal_interview_prompt};

    #[test]
    fn goal_interview_prompt_mentions_goal_file_and_confirmation_gate() {
        let prompt = workflow_goal_interview_prompt(Utf8Path::new("/tmp/demo/GOAL.md"));

        assert!(prompt.contains("`/tmp/demo/GOAL.md`"));
        assert!(prompt.contains("Read `/tmp/demo/GOAL.md` before doing anything else."));
        assert!(prompt.contains("Do not edit `/tmp/demo/GOAL.md` until the user has explicitly confirmed the final direction."));
    }

    #[test]
    fn plan_driven_status_marks_goal_change_as_stale_after_planning() {
        let temp = tempfile::tempdir().unwrap();
        let project_dir = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();
        let store = TargetStore::new(project_dir.clone());
        let target_dir = project_dir.join(".ralph/targets/demo");
        std::fs::create_dir_all(&target_dir).unwrap();
        std::fs::write(target_dir.join("GOAL.md"), "# Goal\n\nChanged\n").unwrap();
        std::fs::write(target_dir.join("plan.toml"), "version = 1\n").unwrap();

        let config = TargetConfig {
            id: "demo".to_owned(),
            scaffold: None,
            mode: Some(WorkflowMode::PlanDriven),
            workflow: Some(PlanDrivenWorkflowState {
                phase: PlanDrivenPhase::Paused,
                last_goal_hash: Some("sha256:old".to_owned()),
                last_planned_at: Some(1),
                ..PlanDrivenWorkflowState::default()
            }),
            inflight: None,
            created_at: None,
            max_iterations: None,
            last_prompt: None,
            last_run_status: LastRunStatus::NeverRun,
        };
        let hashes = plan_driven_hashes(&store, &target_dir).unwrap();
        let status = plan_driven_workflow_status(&config, &hashes, &target_dir);

        assert_eq!(status.derived_state, WorkflowDerivedState::Stale);
        assert_eq!(status.run_advice, WorkflowRunAdvice::Choose);
    }
}
