use std::fs;

use anyhow::{Context, Result};
use camino::Utf8Path;
use ralph_core::{AgentConfig, WorkflowDefinition, WorkflowRunSummary, WorkflowSummary};

pub(crate) struct AgentListRow {
    pub(crate) agent: String,
    pub(crate) detected: bool,
    pub(crate) command: String,
}

pub(crate) struct AgentCurrentRow {
    pub(crate) effective_agent: String,
    pub(crate) project_dir: String,
}

pub(crate) fn print_agent_list(rows: &[AgentListRow]) {
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
    println!("{text}");
}

pub(crate) fn print_agent_current(row: &AgentCurrentRow) {
    println!(
        "effective_agent={}\nproject_dir={}",
        row.effective_agent, row.project_dir
    );
}

pub(crate) fn print_bare_file(path: &Utf8Path) -> Result<()> {
    let contents = fs::read_to_string(path).with_context(|| format!("failed to read {path}"))?;
    println!("{contents}");
    Ok(())
}

pub(crate) fn print_emitted_event(event: &str) {
    println!("event {event} emitted.");
}

pub(crate) fn print_workflow_run(summary: &WorkflowRunSummary) {
    println!(
        "{} [{}] prompt={} run_dir={}",
        summary.workflow_id,
        summary.status.label(),
        summary.final_prompt_id,
        summary.run_dir
    );
}

pub(crate) fn print_workflow_list(workflows: Vec<WorkflowSummary>) {
    let rows = workflows
        .into_iter()
        .map(|workflow| {
            (
                workflow.workflow_id,
                workflow.title,
                workflow.description,
                workflow.path.to_string(),
            )
        })
        .collect::<Vec<_>>();
    let text = if rows.is_empty() {
        "No workflows.".to_owned()
    } else {
        rows.iter()
            .map(|row| {
                if row.2.trim().is_empty() {
                    format!("{} [{}]", row.0, row.3)
                } else {
                    format!("{} - {} [{}]", row.0, row.2, row.3)
                }
            })
            .collect::<Vec<_>>()
            .join("\n")
    };
    println!("{text}");
}

pub(crate) fn print_workflow_definition(workflow: &WorkflowDefinition) -> Result<()> {
    if let Some(path) = workflow.source_path() {
        return print_bare_file(path);
    }
    println!("{}", serde_yaml::to_string(workflow)?);
    Ok(())
}

pub(crate) fn agent_list_rows(agents: &[AgentConfig]) -> Vec<AgentListRow> {
    agents
        .iter()
        .filter(|agent| !agent.hidden)
        .map(|agent| AgentListRow {
            agent: format!("{} ({})", agent.name, agent.id),
            detected: agent.is_available(),
            command: agent.non_interactive.command_preview(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::agent_list_rows;

    #[test]
    fn agent_list_rows_hide_internal_agents() {
        let agents = ralph_core::builtin_agents();
        let rows = agent_list_rows(&agents);
        assert!(rows.iter().all(|row| !row.agent.contains("__test_shell")));
    }
}
