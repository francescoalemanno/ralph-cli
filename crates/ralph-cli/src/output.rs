use std::fs;

use anyhow::{Context, Result, anyhow};
use camino::Utf8Path;
use ralph_core::{AgentConfig, TargetReview, TargetSummary};
use serde::Serialize;

use crate::cli::OutputArg;

#[derive(Debug, Serialize)]
pub(crate) struct AgentListRow {
    pub(crate) agent: String,
    pub(crate) detected: bool,
    pub(crate) command: String,
}

#[derive(Debug, Serialize)]
pub(crate) struct AgentCurrentRow {
    pub(crate) effective_agent: String,
    pub(crate) project_dir: String,
}

#[derive(Debug, Serialize)]
struct TargetListRow {
    target: String,
    last_prompt: Option<String>,
    last_run_status: String,
    prompts: Vec<String>,
    scaffold: Option<String>,
}

#[derive(Debug, Serialize)]
struct PromptFileRow {
    prompt: String,
    scaffold: Option<String>,
    status: Option<String>,
}

pub(crate) fn print_agent_list(output: OutputArg, rows: &[AgentListRow]) -> Result<()> {
    let text = rows
        .iter()
        .map(|row| {
            format!(
                "{:<9} detected={} command={}",
                row.agent, row.detected, row.command
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    print_json_or_text(output, rows, text)
}

pub(crate) fn print_agent_current(output: OutputArg, row: &AgentCurrentRow) -> Result<()> {
    let text = format!(
        "effective_agent={}\nproject_dir={}",
        row.effective_agent, row.project_dir
    );
    print_json_or_text(output, row, text)
}

pub(crate) fn print_target_summary(output: OutputArg, summary: &TargetSummary) -> Result<()> {
    let row = target_row(summary.clone());
    let text = render_targets_text(&[row]);
    print_json_or_text(output, summary, text)
}

pub(crate) fn print_target_list(output: OutputArg, summaries: Vec<TargetSummary>) -> Result<()> {
    let rows = summaries.into_iter().map(target_row).collect::<Vec<_>>();
    let text = render_targets_text(&rows);
    print_json_or_text(output, &rows, text)
}

pub(crate) fn print_target_review(
    output: OutputArg,
    review: &TargetReview,
    selected_file: Option<&str>,
) -> Result<()> {
    if let Some(file_name) = selected_file {
        let file = review
            .files
            .iter()
            .find(|file| file.name == file_name)
            .ok_or_else(|| {
                anyhow!(
                    "file '{file_name}' not found for target '{}'",
                    review.summary.id
                )
            })?;
        if matches!(output, OutputArg::Json) {
            println!("{}", serde_json::to_string_pretty(file)?);
        } else {
            println!("{}", file.contents);
        }
        return Ok(());
    }

    if matches!(output, OutputArg::Json) {
        println!("{}", serde_json::to_string_pretty(review)?);
        return Ok(());
    }

    for (index, file) in review.files.iter().enumerate() {
        if index > 0 {
            println!();
        }
        println!("## {}", file.name);
        println!("{}", file.contents);
    }
    Ok(())
}

pub(crate) fn print_bare_file(output: OutputArg, path: &Utf8Path) -> Result<()> {
    let contents = fs::read_to_string(path).with_context(|| format!("failed to read {path}"))?;
    let row = serde_json::json!({
        "path": path,
        "contents": contents,
    });
    match output {
        OutputArg::Text => {
            println!("{contents}");
            Ok(())
        }
        OutputArg::Json => {
            println!("{}", serde_json::to_string_pretty(&row)?);
            Ok(())
        }
    }
}

pub(crate) fn print_prompt_file_row(
    output: OutputArg,
    prompt: String,
    scaffold: Option<String>,
    status: Option<String>,
) -> Result<()> {
    let row = PromptFileRow {
        prompt,
        scaffold,
        status,
    };
    let text = match row.status.as_deref() {
        Some(status) => format!("{} [{}]", row.prompt, status),
        None => row.prompt.clone(),
    };
    print_json_or_text(output, &row, text)
}

pub(crate) fn print_json_or_text<T>(output: OutputArg, value: &T, text: String) -> Result<()>
where
    T: Serialize + ?Sized,
{
    match output {
        OutputArg::Text => {
            println!("{text}");
            Ok(())
        }
        OutputArg::Json => {
            println!("{}", serde_json::to_string_pretty(value)?);
            Ok(())
        }
    }
}

pub(crate) fn agent_list_rows(agents: &[AgentConfig]) -> Vec<AgentListRow> {
    agents
        .iter()
        .map(|agent| AgentListRow {
            agent: format!("{} ({})", agent.name, agent.id),
            detected: agent.is_available(),
            command: agent.non_interactive.command_preview(),
        })
        .collect()
}

fn target_row(summary: TargetSummary) -> TargetListRow {
    TargetListRow {
        target: summary.id,
        last_prompt: summary.last_prompt,
        last_run_status: summary.last_run_status.label().to_owned(),
        prompts: summary
            .prompt_files
            .into_iter()
            .map(|prompt| prompt.name)
            .collect(),
        scaffold: summary
            .scaffold
            .map(|scaffold| scaffold.as_str().to_owned()),
    }
}

fn render_targets_text(rows: &[TargetListRow]) -> String {
    if rows.is_empty() {
        return "No targets.".to_owned();
    }
    rows.iter()
        .map(|row| {
            format!(
                "{} [{}] prompts={}{}",
                row.target,
                row.last_run_status,
                row.prompts.join(", "),
                row.scaffold
                    .as_ref()
                    .map(|scaffold| format!(" scaffold={scaffold}"))
                    .unwrap_or_default()
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}
