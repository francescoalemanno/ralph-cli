use std::{
    env,
    io::{self, BufRead, IsTerminal, Write},
    process::Command,
    sync::OnceLock,
};

use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use camino::Utf8Path;
use ralph_core::{LastRunStatus, TerminalTheme, ThemeConfig};
use termimad::MadSkin;

use crate::{
    PlanningAnswerSource, PlanningDraftDecision, PlanningDraftDecisionKind, PlanningDraftReview,
    PlanningQuestion, PlanningQuestionAnswer, RunDelegate, RunEvent,
};

#[derive(Debug, Clone, Copy)]
pub struct ConsoleDelegate {
    theme: TerminalTheme,
}

impl ConsoleDelegate {
    pub fn new(theme_config: &ThemeConfig) -> Self {
        Self {
            theme: TerminalTheme::new(theme_config),
        }
    }
}

impl Default for ConsoleDelegate {
    fn default() -> Self {
        Self::new(&ThemeConfig::default())
    }
}

#[async_trait]
impl RunDelegate for ConsoleDelegate {
    async fn on_event(&mut self, event: RunEvent) -> Result<()> {
        match event {
            RunEvent::IterationStarted {
                prompt_name,
                iteration,
                max_iterations,
            } => {
                println!();
                println!(
                    "{}",
                    format_iteration_banner(self.theme, &prompt_name, iteration, max_iterations)
                );
            }
            RunEvent::Output(chunk) => {
                print!("{chunk}");
            }
            RunEvent::ParallelWorkerLaunched { channel_id, label } => {
                println!(
                    "{}",
                    format_parallel_event(self.theme, "queued", &channel_id, &label, None)
                );
            }
            RunEvent::ParallelWorkerStarted { channel_id, label } => {
                println!(
                    "{}",
                    format_parallel_event(self.theme, "running", &channel_id, &label, None)
                );
            }
            RunEvent::ParallelWorkerFinished {
                channel_id,
                label,
                exit_code,
            } => {
                println!(
                    "{}",
                    format_parallel_event(self.theme, "done", &channel_id, &label, Some(exit_code))
                );
            }
            RunEvent::Note(note) => {
                eprintln!("{}", format_note(self.theme, &note));
            }
            RunEvent::Finished { status, summary } => {
                println!("\n{}", format_finish_line(self.theme, status, &summary));
            }
        }
        Ok(())
    }

    async fn answer_planning_question(
        &mut self,
        question: &PlanningQuestion,
    ) -> Result<PlanningQuestionAnswer> {
        let mut stdout = io::stdout().lock();
        writeln!(stdout)?;
        writeln!(stdout, "Planner question")?;
        writeln!(stdout, "{}", question.question)?;
        if let Some(context) = &question.context
            && !context.trim().is_empty()
        {
            writeln!(stdout)?;
            writeln!(stdout, "Context: {}", context.trim())?;
        }
        writeln!(stdout)?;
        for (index, option) in question.options.iter().enumerate() {
            writeln!(stdout, "  {}) {}", index + 1, option)?;
        }
        writeln!(
            stdout,
            "  {}) Other (type your own answer)",
            question.options.len() + 1
        )?;
        stdout.flush()?;
        drop(stdout);

        loop {
            let selection = prompt_line(&format!(
                "Enter number (1-{}): ",
                question.options.len() + 1
            ))?;
            let Ok(selected) = selection.parse::<usize>() else {
                eprintln!("invalid selection, enter a number");
                continue;
            };
            if selected == 0 || selected > question.options.len() + 1 {
                eprintln!(
                    "invalid selection, enter a number between 1 and {}",
                    question.options.len() + 1
                );
                continue;
            }
            if selected == question.options.len() + 1 {
                return Ok(PlanningQuestionAnswer {
                    answer: prompt_nonempty("Enter your answer: ")?,
                    source: PlanningAnswerSource::Custom,
                });
            }
            return Ok(PlanningQuestionAnswer {
                answer: question.options[selected - 1].clone(),
                source: PlanningAnswerSource::Option,
            });
        }
    }

    async fn review_planning_draft(
        &mut self,
        draft: &PlanningDraftReview,
    ) -> Result<PlanningDraftDecision> {
        let mut stdout = io::stdout().lock();
        writeln!(stdout)?;
        writeln!(stdout, "Plan draft")?;
        writeln!(stdout, "Target: {}", draft.target_path)?;
        writeln!(stdout, "--------------------")?;
        render_markdown_draft(&mut stdout, &draft.draft)?;
        writeln!(stdout, "--------------------")?;
        writeln!(stdout)?;
        writeln!(stdout, "  1) Accept")?;
        writeln!(stdout, "  2) Revise")?;
        writeln!(stdout, "  3) Reject")?;
        stdout.flush()?;
        drop(stdout);

        loop {
            match prompt_line("Enter number (1-3): ")?.as_str() {
                "1" => {
                    return Ok(PlanningDraftDecision {
                        kind: PlanningDraftDecisionKind::Accept,
                        feedback: None,
                    });
                }
                "2" => {
                    return Ok(PlanningDraftDecision {
                        kind: PlanningDraftDecisionKind::Revise,
                        feedback: Some(prompt_nonempty("Enter revision feedback: ")?),
                    });
                }
                "3" => {
                    return Ok(PlanningDraftDecision {
                        kind: PlanningDraftDecisionKind::Reject,
                        feedback: None,
                    });
                }
                _ => {
                    eprintln!("invalid selection, enter 1, 2, or 3");
                }
            }
        }
    }
}

pub fn edit_file(path: &Utf8Path, editor_override: Option<&str>) -> Result<()> {
    let editor = preferred_external_editor(editor_override).ok_or_else(|| {
        anyhow!("no editor configured; set project `editor_override`, `VISUAL`, or `EDITOR`")
    })?;
    edit_file_with_external_editor(path, &editor)
}

pub fn prompt_nonempty(prompt: &str) -> Result<String> {
    loop {
        let line = prompt_line(prompt)?;
        if !line.trim().is_empty() {
            return Ok(line);
        }
        eprintln!("input cannot be empty");
    }
}

pub fn prompt_yes_no(prompt: &str, default_yes: bool) -> Result<bool> {
    let suffix = if default_yes { "[Y/n]" } else { "[y/N]" };

    loop {
        let answer = prompt_line(&format!("{prompt}{suffix}: "))?;
        let normalized = answer.trim().to_ascii_lowercase();
        match normalized.as_str() {
            "" => return Ok(default_yes),
            "y" | "yes" => return Ok(true),
            "n" | "no" => return Ok(false),
            _ => eprintln!("enter y or n"),
        }
    }
}

pub fn prompt_line(prompt: &str) -> Result<String> {
    let mut stdout = io::stdout().lock();
    write!(stdout, "{prompt}")?;
    stdout.flush()?;
    drop(stdout);

    let stdin = io::stdin();
    let mut input = String::new();
    stdin.lock().read_line(&mut input)?;
    Ok(input.trim().to_owned())
}

fn render_markdown_draft(stdout: &mut impl Write, markdown: &str) -> Result<()> {
    if io::stdout().is_terminal() {
        write!(stdout, "{}", planning_draft_skin().term_text(markdown))?;
        if !markdown.ends_with('\n') {
            writeln!(stdout)?;
        }
        return Ok(());
    }

    writeln!(stdout, "{markdown}")?;
    if !markdown.ends_with('\n') {
        writeln!(stdout)?;
    }
    Ok(())
}

fn planning_draft_skin() -> &'static MadSkin {
    static SKIN: OnceLock<MadSkin> = OnceLock::new();
    SKIN.get_or_init(MadSkin::default)
}

fn preferred_external_editor(editor_override: Option<&str>) -> Option<String> {
    let visual = env::var("VISUAL").ok();
    let editor = env::var("EDITOR").ok();
    resolve_editor_command(editor_override, visual.as_deref(), editor.as_deref())
}

fn resolve_editor_command(
    editor_override: Option<&str>,
    visual: Option<&str>,
    editor: Option<&str>,
) -> Option<String> {
    [editor_override, visual, editor]
        .into_iter()
        .flatten()
        .map(str::trim)
        .find(|value| !value.is_empty())
        .map(str::to_owned)
}

fn edit_file_with_external_editor(path: &Utf8Path, editor: &str) -> Result<()> {
    let command = format!("{editor} {}", quote_shell_arg(path.as_str()));
    let status = if cfg!(windows) {
        Command::new("cmd").arg("/C").arg(&command).status()
    } else {
        Command::new("sh").arg("-lc").arg(&command).status()
    }
    .with_context(|| format!("failed to launch editor command '{editor}'"))?;

    if status.success() {
        Ok(())
    } else {
        Err(anyhow!(
            "editor command '{editor}' exited with status {status}"
        ))
    }
}

fn quote_shell_arg(value: &str) -> String {
    if cfg!(windows) {
        format!("\"{}\"", value.replace('"', "\\\""))
    } else {
        format!("'{}'", value.replace('\'', "'\"'\"'"))
    }
}

fn format_iteration_banner(
    theme: TerminalTheme,
    prompt_name: &str,
    iteration: usize,
    max_iterations: usize,
) -> String {
    let title = format!(" {prompt_name} | iteration {iteration}/{max_iterations} ");
    let width = title.len().max(72);
    theme
        .style()
        .fg(theme.palette().accent)
        .bold()
        .paint(format!("{title:=^width$}", width = width))
}

fn format_parallel_event(
    theme: TerminalTheme,
    kind: &str,
    channel_id: &str,
    label: &str,
    exit_code: Option<i32>,
) -> String {
    let style = match (kind, exit_code) {
        ("done", Some(0)) => theme.style().fg(theme.palette().success),
        ("done", Some(_)) => theme.style().fg(theme.palette().error),
        ("running", _) => theme.style().fg(theme.palette().accent),
        _ => theme.label_style(),
    };

    match exit_code {
        Some(exit_code) => style.paint(format!(
            "[parallel:{channel_id}] {kind} {label} (exit={exit_code})"
        )),
        None => style.paint(format!("[parallel:{channel_id}] {kind} {label}")),
    }
}

fn format_note(theme: TerminalTheme, note: &str) -> String {
    theme
        .style()
        .fg(theme.palette().warning)
        .paint(format!("! {note}"))
}

fn format_finish_line(theme: TerminalTheme, status: LastRunStatus, summary: &str) -> String {
    theme
        .style()
        .fg(theme.status_color(status))
        .bold()
        .paint(format!("{summary} ({})", status.label()))
}

#[cfg(test)]
mod tests {
    use super::resolve_editor_command;

    #[test]
    fn editor_override_has_highest_priority() {
        assert_eq!(
            resolve_editor_command(Some("nvim"), Some("vim"), Some("nano")),
            Some("nvim".to_owned())
        );
    }

    #[test]
    fn editor_resolution_skips_blank_values() {
        assert_eq!(
            resolve_editor_command(Some("   "), Some(" code -w "), Some("nano")),
            Some("code -w".to_owned())
        );
        assert_eq!(
            resolve_editor_command(None, Some("   "), Some(" nano ")),
            Some("nano".to_owned())
        );
        assert_eq!(resolve_editor_command(None, None, Some("   ")), None);
    }
}
