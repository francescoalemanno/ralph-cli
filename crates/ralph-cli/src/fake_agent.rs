use std::{env, fs};

use anyhow::{Context, Result, anyhow};
use camino::{Utf8Path, Utf8PathBuf};
use toml::Value;

use crate::cli::FakeAgentCommand;

const PLAN_TEMPLATE: &str = r#"version = 1

[[items]]
category = "functional"
description = "Create smoke target entrypoint script"
steps = ["Write script", "Make script executable"]
completed = false

[[items]]
category = "functional"
description = "Verify smoke target executes successfully"
steps = ["Execute smoke target script", "Verify smoke_ran.txt was created", "Verify exit code is 0"]
completed = false
"#;

const PROGRESS_TEMPLATE: &str = r#"version = 1

[[items]]
description = "Create smoke target entrypoint script"
steps = ["Write script", "Make script executable"]
completed = false

[[items]]
description = "Verify smoke target executes successfully"
steps = ["Execute smoke target script", "Verify smoke_ran.txt was created", "Verify exit code is 0"]
completed = false
"#;

const PLAN_SPEC_TEMPLATE: &str = r#"# Smoke Target

This spec exists to validate the Ralph workflow graph quickly.

- Create a tiny smoke entrypoint.
- Write a marker file when it runs.
- Exit successfully.
"#;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ArtifactKind {
    Plan,
    Progress,
    Generic,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FakeAgentInvocation {
    prompt_name: String,
    target_dir: Utf8PathBuf,
    prompt_path: Option<Utf8PathBuf>,
    prompt_file: Option<Utf8PathBuf>,
    goal_path: Option<Utf8PathBuf>,
}

impl FakeAgentInvocation {
    fn from_env() -> Result<Self> {
        Ok(Self {
            prompt_name: env::var("RALPH_PROMPT_NAME")
                .context("fake agent requires RALPH_PROMPT_NAME")?,
            target_dir: env_utf8_path("RALPH_TARGET_DIR")?,
            prompt_path: env::var("RALPH_PROMPT_PATH").ok().map(Utf8PathBuf::from),
            prompt_file: env::var("RALPH_PROMPT_FILE").ok().map(Utf8PathBuf::from),
            goal_path: env::var("RALPH_GOAL_PATH").ok().map(Utf8PathBuf::from),
        })
    }

    fn prompt_source(&self) -> String {
        self.prompt_path
            .as_ref()
            .filter(|path| path.exists())
            .and_then(|path| fs::read_to_string(path).ok())
            .or_else(|| {
                self.prompt_file
                    .as_ref()
                    .filter(|path| path.exists())
                    .and_then(|path| fs::read_to_string(path).ok())
            })
            .unwrap_or_default()
    }
}

pub(crate) fn run(command: FakeAgentCommand) -> Result<()> {
    let invocation = FakeAgentInvocation::from_env()?;
    match command {
        FakeAgentCommand::Run => run_non_interactive(&invocation),
        FakeAgentCommand::Interactive => run_interactive(&invocation),
    }
}

fn run_non_interactive(invocation: &FakeAgentInvocation) -> Result<()> {
    let prompt_source = invocation.prompt_source();
    let kind = detect_artifact_kind(&invocation.prompt_name, &prompt_source);
    if is_build_like(&invocation.prompt_name, &prompt_source) {
        match kind {
            ArtifactKind::Plan => {
                if !invocation.target_dir.join("plan.toml").exists() {
                    seed_plan_artifacts(&invocation.target_dir)?;
                }
                let advanced = complete_next_item(&invocation.target_dir.join("plan.toml"))?;
                write_smoke_marker(&invocation.target_dir, "plan")?;
                append_journal(
                    &invocation.target_dir,
                    if advanced {
                        "fake-agent: completed one plan item"
                    } else {
                        "fake-agent: plan already complete"
                    },
                )?;
                println!("fake-agent: build step exercised plan.toml");
            }
            ArtifactKind::Progress => {
                if !invocation.target_dir.join("progress.toml").exists() {
                    seed_progress_artifacts(&invocation.target_dir)?;
                }
                let advanced = complete_next_item(&invocation.target_dir.join("progress.toml"))?;
                write_smoke_marker(&invocation.target_dir, "progress")?;
                append_journal(
                    &invocation.target_dir,
                    if advanced {
                        "fake-agent: completed one progress item"
                    } else {
                        "fake-agent: progress already complete"
                    },
                )?;
                println!("fake-agent: build step exercised progress.toml");
            }
            ArtifactKind::Generic => {
                write_generic_probe(&invocation.target_dir, &invocation.prompt_name)?;
                println!("fake-agent: generic build probe updated");
            }
        }
        return Ok(());
    }

    match kind {
        ArtifactKind::Plan => {
            seed_plan_artifacts(&invocation.target_dir)?;
            append_journal(
                &invocation.target_dir,
                "fake-agent: seeded plan graph artifacts",
            )?;
            println!("fake-agent: seeded plan.toml and specs/");
        }
        ArtifactKind::Progress => {
            seed_progress_artifacts(&invocation.target_dir)?;
            append_journal(
                &invocation.target_dir,
                "fake-agent: seeded progress graph artifacts",
            )?;
            println!("fake-agent: seeded progress.toml");
        }
        ArtifactKind::Generic => {
            write_generic_probe(&invocation.target_dir, &invocation.prompt_name)?;
            println!("fake-agent: generic probe updated");
        }
    }

    Ok(())
}

fn run_interactive(invocation: &FakeAgentInvocation) -> Result<()> {
    if let Some(goal_path) = &invocation.goal_path {
        let existing = fs::read_to_string(goal_path).unwrap_or_default();
        let marker = "\n## Fake Agent Note\nRefined by the fake workflow agent.\n";
        if !existing.contains("Refined by the fake workflow agent.") {
            fs::write(goal_path, format!("{existing}{marker}"))
                .with_context(|| format!("failed to update {}", goal_path))?;
        }
        println!("fake-agent: refined {}", goal_path);
        return Ok(());
    }

    println!("fake-agent: no goal file to refine");
    Ok(())
}

fn detect_artifact_kind(prompt_name: &str, prompt_source: &str) -> ArtifactKind {
    let prompt_name = prompt_name.to_ascii_lowercase();
    let prompt_source = prompt_source.to_ascii_lowercase();
    if prompt_name.contains("task_driven") || prompt_source.contains("progress.toml") {
        ArtifactKind::Progress
    } else if prompt_name.contains("plan_driven")
        || prompt_name.contains("_plan")
        || prompt_source.contains("plan.toml")
        || prompt_source.contains("specs/")
    {
        ArtifactKind::Plan
    } else {
        ArtifactKind::Generic
    }
}

fn is_build_like(prompt_name: &str, prompt_source: &str) -> bool {
    let prompt_name = prompt_name.to_ascii_lowercase();
    let prompt_source = prompt_source.to_ascii_lowercase();
    prompt_name.contains("build")
        || prompt_source.contains("select the single highest-priority open item")
        || prompt_source.contains("update `progress.toml`")
        || prompt_source.contains("update `plan.toml`")
}

fn seed_plan_artifacts(target_dir: &Utf8Path) -> Result<()> {
    fs::create_dir_all(target_dir.join("specs"))
        .with_context(|| format!("failed to create {}", target_dir.join("specs")))?;
    fs::write(target_dir.join("specs/smoke_target.md"), PLAN_SPEC_TEMPLATE).with_context(|| {
        format!(
            "failed to write {}",
            target_dir.join("specs/smoke_target.md")
        )
    })?;
    fs::write(target_dir.join("plan.toml"), PLAN_TEMPLATE)
        .with_context(|| format!("failed to write {}", target_dir.join("plan.toml")))?;
    Ok(())
}

fn seed_progress_artifacts(target_dir: &Utf8Path) -> Result<()> {
    fs::write(target_dir.join("progress.toml"), PROGRESS_TEMPLATE)
        .with_context(|| format!("failed to write {}", target_dir.join("progress.toml")))?;
    Ok(())
}

fn complete_next_item(path: &Utf8Path) -> Result<bool> {
    let raw = fs::read_to_string(path).with_context(|| format!("failed to read {}", path))?;
    let mut value: Value =
        toml::from_str(&raw).with_context(|| format!("failed to parse TOML at {}", path))?;
    let table = value
        .as_table_mut()
        .ok_or_else(|| anyhow!("workflow artifact '{}' is not a TOML table", path))?;
    let items = table
        .entry("items")
        .or_insert_with(|| Value::Array(Vec::new()))
        .as_array_mut()
        .ok_or_else(|| {
            anyhow!(
                "workflow artifact '{}' does not contain an items array",
                path
            )
        })?;

    for item in items.iter_mut() {
        let Some(item_table) = item.as_table_mut() else {
            continue;
        };
        let completed = item_table
            .get("completed")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if !completed {
            item_table.insert("completed".to_owned(), Value::Boolean(true));
            fs::write(path, toml::to_string_pretty(&value)?)
                .with_context(|| format!("failed to write {}", path))?;
            return Ok(true);
        }
    }

    fs::write(path, toml::to_string_pretty(&value)?)
        .with_context(|| format!("failed to write {}", path))?;
    Ok(false)
}

fn write_smoke_marker(target_dir: &Utf8Path, label: &str) -> Result<()> {
    fs::write(
        target_dir.join("smoke_ran.txt"),
        format!(
            "fake-agent executed {label} at {}\n",
            chrono_like_timestamp()
        ),
    )
    .with_context(|| format!("failed to write {}", target_dir.join("smoke_ran.txt")))
}

fn write_generic_probe(target_dir: &Utf8Path, prompt_name: &str) -> Result<()> {
    fs::write(
        target_dir.join("fake-agent.log"),
        format!("fake-agent handled prompt '{prompt_name}'\n"),
    )
    .with_context(|| format!("failed to write {}", target_dir.join("fake-agent.log")))
}

fn append_journal(target_dir: &Utf8Path, line: &str) -> Result<()> {
    let journal_path = target_dir.join("journal.txt");
    let existing = fs::read_to_string(&journal_path).unwrap_or_default();
    let rendered = if existing.is_empty() {
        format!("# Journal\n\n- {line}\n")
    } else if existing.ends_with('\n') {
        format!("{existing}- {line}\n")
    } else {
        format!("{existing}\n- {line}\n")
    };
    fs::write(&journal_path, rendered).with_context(|| format!("failed to write {journal_path}"))
}

fn env_utf8_path(key: &str) -> Result<Utf8PathBuf> {
    let value = env::var(key).with_context(|| format!("fake agent requires {key}"))?;
    Ok(Utf8PathBuf::from(value))
}

fn chrono_like_timestamp() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};

    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs().to_string())
        .unwrap_or_else(|_| "0".to_owned())
}

#[cfg(test)]
mod tests {
    use anyhow::Result;
    use camino::Utf8PathBuf;

    use super::{
        ArtifactKind, FakeAgentInvocation, complete_next_item, detect_artifact_kind,
        run_interactive, run_non_interactive,
    };

    #[test]
    fn plan_fake_run_seeds_specs_and_plan() -> Result<()> {
        let temp = tempfile::tempdir().unwrap();
        let target_dir = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();
        let invocation = FakeAgentInvocation {
            prompt_name: "plan_driven_plan".to_owned(),
            target_dir: target_dir.clone(),
            prompt_path: None,
            prompt_file: None,
            goal_path: None,
        };

        run_non_interactive(&invocation)?;

        assert!(target_dir.join("specs/smoke_target.md").exists());
        assert!(target_dir.join("plan.toml").exists());
        assert!(target_dir.join("journal.txt").exists());
        Ok(())
    }

    #[test]
    fn build_fake_run_completes_next_item() -> Result<()> {
        let temp = tempfile::tempdir().unwrap();
        let target_dir = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();
        std::fs::write(target_dir.join("plan.toml"), super::PLAN_TEMPLATE)?;

        let changed = complete_next_item(&target_dir.join("plan.toml"))?;
        let after = std::fs::read_to_string(target_dir.join("plan.toml"))?;

        assert!(changed);
        assert!(after.contains("completed = true"));
        Ok(())
    }

    #[test]
    fn interactive_fake_run_refines_goal() -> Result<()> {
        let temp = tempfile::tempdir().unwrap();
        let target_dir = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();
        let goal_path = target_dir.join("GOAL.md");
        std::fs::write(&goal_path, "# Goal\n\nOriginal\n")?;
        let invocation = FakeAgentInvocation {
            prompt_name: "workflow_goal_interview".to_owned(),
            target_dir,
            prompt_path: None,
            prompt_file: None,
            goal_path: Some(goal_path.clone()),
        };

        run_interactive(&invocation)?;

        let after = std::fs::read_to_string(goal_path)?;
        assert!(after.contains("Refined by the fake workflow agent."));
        Ok(())
    }

    #[test]
    fn prompt_heuristics_detect_progress_flows() {
        assert_eq!(
            detect_artifact_kind("task_driven_build", ""),
            ArtifactKind::Progress
        );
        assert_eq!(
            detect_artifact_kind("custom", "Please update progress.toml"),
            ArtifactKind::Progress
        );
    }
}
