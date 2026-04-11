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

pub(crate) struct CliRunHeader {
    pub(crate) version: &'static str,
    pub(crate) workflow_id: String,
    pub(crate) workflow_title: String,
    pub(crate) entrypoint: String,
    pub(crate) agent: String,
    pub(crate) runner: String,
    pub(crate) project_dir: String,
    pub(crate) branch: Option<String>,
    pub(crate) request_source: String,
    pub(crate) request_preview: Option<String>,
    pub(crate) max_iterations: usize,
    pub(crate) session_timeout_secs: Option<u64>,
    pub(crate) idle_timeout_secs: Option<u64>,
    pub(crate) workflow_options: Vec<(String, String)>,
    pub(crate) artifact_root: String,
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

pub(crate) fn print_workflow_run(summary: &WorkflowRunSummary) {
    const ANSI_BOLD_GREEN: &str = "\x1b[1;32m";
    const ANSI_DIM: &str = "\x1b[2m";
    const ANSI_RESET: &str = "\x1b[0m";
    let title = " Run Result ";
    let width = title.len().max(72);
    println!(
        "{ANSI_BOLD_GREEN}{title:=^width$}{ANSI_RESET}\n{ANSI_DIM}workflow{ANSI_RESET} {} [{}] prompt={}\n{ANSI_DIM}run_dir  {ANSI_RESET} {}",
        summary.workflow_id,
        summary.status.label(),
        summary.final_prompt_id,
        summary.run_dir,
        width = width
    );
}

pub(crate) fn print_run_header(header: &CliRunHeader) {
    println!("{}", render_run_header(header));
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
            command: agent.runner.command_preview(),
        })
        .collect()
}

fn render_run_header(header: &CliRunHeader) -> String {
    const ANSI_BOLD_CYAN: &str = "\x1b[1;36m";
    const ANSI_DIM: &str = "\x1b[2m";
    const ANSI_RESET: &str = "\x1b[0m";

    let title = format!(" RALPH v{} | CLI RUN ", header.version);
    let width = title.len().max(72);
    let mut lines = vec![format!(
        "{ANSI_BOLD_CYAN}{title:=^width$}{ANSI_RESET}",
        width = width
    )];

    lines.push(format!(
        "{ANSI_DIM}workflow  {ANSI_RESET} {} ({})",
        header.workflow_id, header.workflow_title
    ));
    lines.push(format!(
        "{ANSI_DIM}entry     {ANSI_RESET} {}",
        header.entrypoint
    ));
    lines.push(format!("{ANSI_DIM}agent     {ANSI_RESET} {}", header.agent));
    lines.push(format!(
        "{ANSI_DIM}runner    {ANSI_RESET} {}",
        header.runner
    ));
    lines.push(format!(
        "{ANSI_DIM}project   {ANSI_RESET} {}",
        header.project_dir
    ));
    if let Some(branch) = &header.branch {
        lines.push(format!("{ANSI_DIM}branch    {ANSI_RESET} {}", branch));
    }
    lines.push(format!(
        "{ANSI_DIM}request   {ANSI_RESET} {}",
        header.request_source
    ));
    if let Some(preview) = &header.request_preview {
        lines.push(format!("{ANSI_DIM}preview   {ANSI_RESET}"));
        for line in preview_text(preview, 3) {
            lines.push(format!("  {}", line));
        }
    }
    lines.push(format!(
        "{ANSI_DIM}limits    {ANSI_RESET} {}",
        format_limits_line(
            header.max_iterations,
            header.session_timeout_secs,
            header.idle_timeout_secs
        )
    ));
    if !header.workflow_options.is_empty() {
        let options = header
            .workflow_options
            .iter()
            .map(|(key, value)| format!("{key}={value}"))
            .collect::<Vec<_>>()
            .join(" | ");
        lines.push(format!("{ANSI_DIM}options   {ANSI_RESET} {}", options));
    }
    lines.push(format!(
        "{ANSI_DIM}artifacts {ANSI_RESET} {}",
        header.artifact_root
    ));
    lines.push(format!("{ANSI_BOLD_CYAN}{}{ANSI_RESET}", "=".repeat(width)));
    lines.join("\n")
}

fn format_limits_line(
    max_iterations: usize,
    session_timeout_secs: Option<u64>,
    idle_timeout_secs: Option<u64>,
) -> String {
    let mut parts = vec![format!("max {max_iterations} iterations")];
    if let Some(session_timeout_secs) = session_timeout_secs {
        parts.push(format!(
            "session {}",
            format_timeout_duration(session_timeout_secs)
        ));
    }
    if let Some(idle_timeout_secs) = idle_timeout_secs {
        parts.push(format!(
            "idle {}",
            format_timeout_duration(idle_timeout_secs)
        ));
    }
    parts.join(" | ")
}

fn format_timeout_duration(total_seconds: u64) -> String {
    if total_seconds % 3600 == 0 {
        return format!("{}h", total_seconds / 3600);
    }
    if total_seconds % 60 == 0 {
        return format!("{}m", total_seconds / 60);
    }
    format!("{}s", total_seconds)
}

fn preview_text(text: &str, max_lines: usize) -> Vec<String> {
    let lines = text.lines().map(str::to_owned).collect::<Vec<_>>();
    let mut preview = lines.iter().take(max_lines).cloned().collect::<Vec<_>>();
    let omitted = lines.len().saturating_sub(preview.len());
    if omitted > 0 {
        preview.push(format!(
            "... (+{} more line{})",
            omitted,
            if omitted == 1 { "" } else { "s" }
        ));
    }
    if preview.is_empty() {
        preview.push("<empty>".to_owned());
    }
    preview
}

#[cfg(test)]
mod tests {
    use super::{CliRunHeader, agent_list_rows, render_run_header};

    fn strip_ansi(input: &str) -> String {
        let mut stripped = String::new();
        let mut chars = input.chars().peekable();
        while let Some(ch) = chars.next() {
            if ch == '\u{1b}' && chars.peek() == Some(&'[') {
                let _ = chars.next();
                for next in chars.by_ref() {
                    if ('@'..='~').contains(&next) {
                        break;
                    }
                }
                continue;
            }
            stripped.push(ch);
        }
        stripped
    }

    #[test]
    fn agent_list_rows_hide_internal_agents() {
        let agents = ralph_core::builtin_agents();
        let rows = agent_list_rows(&agents);
        assert!(rows.iter().all(|row| !row.agent.contains("__test_shell")));
    }

    #[test]
    fn render_run_header_truncates_request_preview_after_three_lines() {
        let rendered = strip_ansi(&render_run_header(&CliRunHeader {
            version: "0.4.4",
            workflow_id: "plan".to_owned(),
            workflow_title: "Plan".to_owned(),
            entrypoint: "draft".to_owned(),
            agent: "Codex (codex)".to_owned(),
            runner: "codex exec".to_owned(),
            project_dir: "/tmp/project".to_owned(),
            branch: Some("main".to_owned()),
            request_source: "argv".to_owned(),
            request_preview: Some("one\ntwo\nthree\nfour".to_owned()),
            max_iterations: 40,
            session_timeout_secs: Some(3600),
            idle_timeout_secs: Some(600),
            workflow_options: vec![("plans-dir".to_owned(), "docs/plans".to_owned())],
            artifact_root: "/tmp/project/.ralph/runs/plan".to_owned(),
        }));

        assert!(rendered.contains("workflow   plan (Plan)"));
        assert!(rendered.contains("branch     main"));
        assert!(rendered.contains("  one"));
        assert!(rendered.contains("  two"));
        assert!(rendered.contains("  three"));
        assert!(rendered.contains("  ... (+1 more line)"));
        assert!(!rendered.contains("  four"));
    }
}
