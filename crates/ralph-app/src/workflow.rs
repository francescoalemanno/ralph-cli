use std::{
    fs,
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, anyhow};
use camino::{Utf8Path, Utf8PathBuf};
use ralph_core::{GoalDrivenPhase, TargetConfig, TargetStore};
use sha2::{Digest, Sha256};

pub(crate) const GOAL_DRIVEN_GOAL_FILE: &str = "GOAL.md";
const GOAL_DRIVEN_PLAN_FILE: &str = "plan.toml";
const GOAL_DRIVEN_SPECS_DIR: &str = "specs";
pub(crate) const GOAL_DRIVEN_PLAN_PROMPT: &str = "goal_driven_plan";
pub(crate) const GOAL_DRIVEN_BUILD_PROMPT: &str = "goal_driven_build";
pub(crate) const GOAL_DRIVEN_PAUSED_PROMPT: &str = "goal_driven_paused";
pub(crate) const TASK_BASED_PROGRESS_FILE: &str = "progress.toml";
const TASK_BASED_JOURNAL_FILE: &str = "journal.txt";
pub(crate) const TASK_BASED_BUILD_PROMPT: &str = "task_based_build";
pub(crate) const TASK_BASED_PAUSED_PROMPT: &str = "task_based_paused";

const RALPH_ENV_TARGET_DIR: &str = "{ralph-env:TARGET_DIR}";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GoalDrivenAction {
    Plan,
    Build,
    Paused,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GoalDrivenHashes {
    pub(crate) goal_hash: String,
    pub(crate) content_hash: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TaskBasedHashes {
    pub(crate) goal_hash: String,
    pub(crate) content_hash: String,
}

pub(crate) fn select_goal_driven_action(
    config: &TargetConfig,
    hashes: &GoalDrivenHashes,
) -> GoalDrivenAction {
    if let Some(inflight) = &config.inflight {
        return match inflight.phase {
            GoalDrivenPhase::Plan => GoalDrivenAction::Plan,
            GoalDrivenPhase::Build => {
                if inflight.goal_hash == hashes.goal_hash {
                    GoalDrivenAction::Build
                } else {
                    GoalDrivenAction::Plan
                }
            }
            GoalDrivenPhase::Paused => GoalDrivenAction::Paused,
        };
    }

    let workflow = config.workflow.clone().unwrap_or_default();
    if workflow
        .last_goal_hash
        .as_deref()
        .is_none_or(|hash| hash != hashes.goal_hash)
    {
        return GoalDrivenAction::Plan;
    }

    match workflow.phase {
        GoalDrivenPhase::Plan => GoalDrivenAction::Plan,
        GoalDrivenPhase::Build => GoalDrivenAction::Build,
        GoalDrivenPhase::Paused => {
            if workflow.last_content_hash.as_deref() == Some(hashes.content_hash.as_str()) {
                GoalDrivenAction::Paused
            } else {
                GoalDrivenAction::Plan
            }
        }
    }
}

pub(crate) fn select_task_based_build_needed(
    config: &TargetConfig,
    hashes: &TaskBasedHashes,
) -> bool {
    if config.inflight.is_some() {
        return true;
    }

    let workflow = config.workflow.clone().unwrap_or_default();
    if workflow
        .last_goal_hash
        .as_deref()
        .is_none_or(|hash| hash != hashes.goal_hash)
    {
        return true;
    }

    match workflow.phase {
        GoalDrivenPhase::Paused => {
            workflow.last_content_hash.as_deref() != Some(hashes.content_hash.as_str())
        }
        GoalDrivenPhase::Plan | GoalDrivenPhase::Build => true,
    }
}

pub(crate) fn goal_driven_hashes(
    store: &TargetStore,
    target_dir: &Utf8Path,
) -> Result<GoalDrivenHashes> {
    let goal_path = target_dir.join(GOAL_DRIVEN_GOAL_FILE);
    let goal_contents = store
        .read_file(&goal_path)
        .with_context(|| format!("missing required goal file {}", goal_path))?;
    let goal_hash = hash_bytes(goal_contents.as_bytes());

    let mut hasher = Sha256::new();
    hash_named_contents(&mut hasher, GOAL_DRIVEN_GOAL_FILE, goal_contents.as_bytes());

    let plan_path = target_dir.join(GOAL_DRIVEN_PLAN_FILE);
    if plan_path.exists() {
        hash_named_contents(
            &mut hasher,
            GOAL_DRIVEN_PLAN_FILE,
            store.read_file(&plan_path)?.as_bytes(),
        );
    }

    let specs_dir = target_dir.join(GOAL_DRIVEN_SPECS_DIR);
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

    Ok(GoalDrivenHashes {
        goal_hash,
        content_hash: format!("sha256:{:x}", hasher.finalize()),
    })
}

pub(crate) fn task_based_hashes(
    store: &TargetStore,
    target_dir: &Utf8Path,
) -> Result<TaskBasedHashes> {
    let goal_path = target_dir.join(GOAL_DRIVEN_GOAL_FILE);
    let goal_contents = store
        .read_file(&goal_path)
        .with_context(|| format!("missing required goal file {}", goal_path))?;
    let goal_hash = hash_bytes(goal_contents.as_bytes());

    let mut hasher = Sha256::new();
    hash_named_contents(&mut hasher, GOAL_DRIVEN_GOAL_FILE, goal_contents.as_bytes());

    let progress_path = target_dir.join(TASK_BASED_PROGRESS_FILE);
    if progress_path.exists() {
        hash_named_contents(
            &mut hasher,
            TASK_BASED_PROGRESS_FILE,
            store.read_file(&progress_path)?.as_bytes(),
        );
    }

    Ok(TaskBasedHashes {
        goal_hash,
        content_hash: format!("sha256:{:x}", hasher.finalize()),
    })
}

pub(crate) fn goal_driven_plan_prompt() -> String {
    format!(
        r#"1. Study these inputs before planning:
   a. Study `{target_dir}/GOAL.md`.
   b. Study `{target_dir}/plan.toml` if it exists.
   c. Study all spec files in `{target_dir}/specs/`.
2. Study the relevant repository documentation and source code (do not assume something is not implemented, look deeply).
3. Create or revise the spec files in `{target_dir}/specs/` until the functional requirements, non-functional requirements, constraints, and user-visible outcomes are clear enough to build against without guessing.
4. Only after the specifications are coherent and sufficient, create or revise `{target_dir}/plan.toml` as the current operational plan.
5. `plan.toml` must stay valid TOML and follow this exact shape:

```toml
version = 1

[[items]]
category = "functional" # or "non_functional"
description = "Describe one concrete outcome"
steps = ["List the ordered implementation and verification steps"]
completed = false
```

6. Keep the items in the exact execution order. Earlier items must prepare later items; do not rely on later items to make earlier items possible.
7. Use `category` to distinguish functional and non-functional work when relevant.
8. Keep every incomplete item at `completed = false`.
9. Plan only. Do not implement product code or tests.
10. If the specifications and `plan.toml` are already correct and sufficient, leave `{target_dir}/plan.toml` unchanged.

{{"ralph":"watch","path":"{target_dir}/plan.toml"}}
"#,
        target_dir = RALPH_ENV_TARGET_DIR
    )
}

pub(crate) fn goal_driven_build_prompt() -> String {
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

pub(crate) fn task_based_build_prompt() -> String {
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
        journal_file = TASK_BASED_JOURNAL_FILE
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
