use std::fs;

use anyhow::{Context, Result};
use camino::Utf8Path;
use ralph_core::{
    AgentConfig, TerminalTheme, ThemeConfig, WorkflowDefinition, WorkflowRunSummary,
    WorkflowSummary, format_timeout_duration,
};

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

pub(crate) fn print_bare_file(path: &Utf8Path) -> Result<()> {
    let contents = fs::read_to_string(path).with_context(|| format!("failed to read {path}"))?;
    println!("{contents}");
    Ok(())
}

pub(crate) fn print_workflow_run(theme_config: &ThemeConfig, summary: &WorkflowRunSummary) {
    let theme = TerminalTheme::new(theme_config);
    let title = " Run Result ";
    let width = title.len().max(72);
    println!(
        "{}\n{} {} [{}] prompt={}\n{} {}",
        theme
            .style()
            .fg(theme.status_color(summary.status))
            .bold()
            .paint(format!("{title:=^width$}", width = width)),
        theme.label_style().paint("workflow"),
        summary.workflow_id,
        summary.status.label(),
        summary.final_prompt_id,
        theme.label_style().paint("run_dir  "),
        summary.run_dir,
    );
}

pub(crate) fn print_run_header(theme_config: &ThemeConfig, header: &CliRunHeader) {
    println!("{}", render_run_header(theme_config, header));
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

pub(crate) fn print_agent_list(
    configured_agent_id: &str,
    effective_agent_id: &str,
    all_agents: &[AgentConfig],
    available_agents: &[&AgentConfig],
) {
    println!(
        "{}",
        render_agent_list(
            configured_agent_id,
            effective_agent_id,
            all_agents,
            available_agents
        )
    );
}

pub(crate) fn print_workflow_definition(workflow: &WorkflowDefinition) -> Result<()> {
    if let Some(path) = workflow.source_path() {
        return print_bare_file(path);
    }
    println!("{}", serde_yaml::to_string(workflow)?);
    Ok(())
}

fn render_run_header(theme_config: &ThemeConfig, header: &CliRunHeader) -> String {
    let theme = TerminalTheme::new(theme_config);
    let title = format!(" RALPH v{} | RUN ", header.version);
    let width = title.len().max(72);
    let mut lines = vec![
        theme
            .style()
            .fg(theme.palette().accent)
            .bold()
            .paint(format!("{title:=^width$}", width = width)),
    ];

    lines.push(format!(
        "{} {} ({})",
        theme.label_style().paint("workflow  "),
        header.workflow_id,
        header.workflow_title
    ));
    lines.push(format!(
        "{} {}",
        theme.label_style().paint("entry     "),
        header.entrypoint
    ));
    lines.push(format!(
        "{} {}",
        theme.label_style().paint("agent     "),
        header.agent
    ));
    lines.push(format!(
        "{} {}",
        theme.label_style().paint("runner    "),
        header.runner
    ));
    lines.push(format!(
        "{} {}",
        theme.label_style().paint("project   "),
        header.project_dir
    ));
    if let Some(branch) = &header.branch {
        lines.push(format!(
            "{} {}",
            theme.label_style().paint("branch    "),
            branch
        ));
    }
    lines.push(format!(
        "{} {}",
        theme.label_style().paint("request   "),
        header.request_source
    ));
    if let Some(preview) = &header.request_preview {
        lines.push(theme.label_style().paint("preview   "));
        for line in preview_text(preview, 3) {
            lines.push(format!("  {}", line));
        }
    }
    lines.push(format!(
        "{} {}",
        theme.label_style().paint("limits    "),
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
        lines.push(format!(
            "{} {}",
            theme.label_style().paint("options   "),
            options
        ));
    }
    lines.push(format!(
        "{} {}",
        theme.label_style().paint("artifacts "),
        header.artifact_root
    ));
    lines.push(
        theme
            .style()
            .fg(theme.palette().accent)
            .bold()
            .paint("=".repeat(width)),
    );
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

fn render_agent_list(
    configured_agent_id: &str,
    effective_agent_id: &str,
    all_agents: &[AgentConfig],
    available_agents: &[&AgentConfig],
) -> String {
    let visible_agents = all_agents
        .iter()
        .filter(|agent| !agent.hidden)
        .collect::<Vec<_>>();

    let configured = format_agent_identity(
        configured_agent_id,
        all_agents
            .iter()
            .find(|agent| agent.id == configured_agent_id),
    );
    let effective = format_agent_identity(
        effective_agent_id,
        all_agents
            .iter()
            .find(|agent| agent.id == effective_agent_id),
    );

    let mut lines = vec![
        format!("configured  {configured}"),
        format!("effective    {effective}"),
        String::new(),
        "available".to_owned(),
    ];

    if available_agents.is_empty() {
        lines.push("<none>".to_owned());
    } else {
        lines.extend(
            available_agents
                .iter()
                .map(|agent| format_agent_identity(&agent.id, Some(agent))),
        );
    }

    lines.push(String::new());
    lines.push("configured registry".to_owned());
    if visible_agents.is_empty() {
        lines.push("<none>".to_owned());
    } else {
        lines.extend(visible_agents.into_iter().map(|agent| {
            let mut markers = vec![if agent.is_available() {
                "available".to_owned()
            } else {
                "unavailable".to_owned()
            }];
            if agent.id == configured_agent_id {
                markers.push("configured".to_owned());
            }
            if agent.id == effective_agent_id {
                markers.push("effective".to_owned());
            }
            format!(
                "{} [{}]",
                format_agent_identity(&agent.id, Some(agent)),
                markers.join(", ")
            )
        }));
    }

    lines.join("\n")
}

fn format_agent_identity(agent_id: &str, agent: Option<&AgentConfig>) -> String {
    agent
        .map(|agent| format!("{} ({})", agent.id, agent.name))
        .unwrap_or_else(|| agent_id.to_owned())
}

#[cfg(test)]
mod tests {
    use super::{CliRunHeader, render_agent_list, render_run_header};
    use ralph_core::{AgentConfig, CommandMode, PromptInput, RunnerConfig, ThemeConfig};

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
    fn render_run_header_truncates_request_preview_after_three_lines() {
        let rendered = strip_ansi(&render_run_header(
            &ThemeConfig::default(),
            &CliRunHeader {
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
            },
        ));

        assert!(rendered.contains("workflow   plan (Plan)"));
        assert!(rendered.contains("branch     main"));
        assert!(rendered.contains("  one"));
        assert!(rendered.contains("  two"));
        assert!(rendered.contains("  three"));
        assert!(rendered.contains("  ... (+1 more line)"));
        assert!(!rendered.contains("  four"));
    }

    #[test]
    fn render_agent_list_marks_availability_and_effective_fallback() {
        let missing = AgentConfig {
            id: "missing".to_owned(),
            name: "Missing".to_owned(),
            builtin: false,
            hidden: false,
            runner: RunnerConfig {
                mode: CommandMode::Exec,
                program: Some("/definitely/missing".to_owned()),
                args: Vec::new(),
                command: None,
                prompt_input: PromptInput::Stdin,
                prompt_env_var: "PROMPT".to_owned(),
                env: Default::default(),
                session_timeout_secs: None,
                idle_timeout_secs: None,
            },
        };
        let working = AgentConfig {
            id: "working".to_owned(),
            name: "Working".to_owned(),
            builtin: false,
            hidden: false,
            runner: RunnerConfig {
                mode: CommandMode::Shell,
                program: None,
                args: Vec::new(),
                command: Some("echo hi".to_owned()),
                prompt_input: PromptInput::Argv,
                prompt_env_var: "PROMPT".to_owned(),
                env: Default::default(),
                session_timeout_secs: None,
                idle_timeout_secs: None,
            },
        };
        let rendered = render_agent_list(
            "missing",
            "working",
            &[missing.clone(), working.clone()],
            &[&working],
        );

        assert!(rendered.contains("configured  missing (Missing)"));
        assert!(rendered.contains("effective    working (Working)"));
        assert!(rendered.contains("available\nworking (Working)"));
        assert!(rendered.contains("missing (Missing) [unavailable, configured]"));
        assert!(rendered.contains("working (Working) [available, effective]"));
    }
}
