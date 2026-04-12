use std::{
    env,
    fs::{File, OpenOptions},
    io::{self, BufRead, IsTerminal, Write},
    process::Command,
};

use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use camino::Utf8Path;
use ralph_core::{LastRunStatus, TerminalTheme, ThemeColor, ThemeConfig};
use termimad::{MadSkin, crossterm::style::Color};

use crate::{
    PlanningAnswerSource, PlanningDraftDecision, PlanningDraftDecisionKind, PlanningDraftReview,
    PlanningQuestion, PlanningQuestionAnswer, RunDelegate, RunEvent,
};

#[derive(Debug, Clone, Copy)]
pub struct ConsoleDelegate {
    theme: TerminalTheme,
}

#[cfg(unix)]
const CONSOLE_INPUT_PATH: &str = "/dev/tty";
#[cfg(unix)]
const CONSOLE_OUTPUT_PATH: &str = "/dev/tty";

#[cfg(windows)]
const CONSOLE_INPUT_PATH: &str = "CONIN$";
#[cfg(windows)]
const CONSOLE_OUTPUT_PATH: &str = "CONOUT$";

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
        with_prompt_output(|output, _| {
            writeln!(output)?;
            writeln!(output, "Planner question")?;
            writeln!(output, "{}", question.question)?;
            if let Some(context) = &question.context
                && !context.trim().is_empty()
            {
                writeln!(output)?;
                writeln!(output, "Context: {}", context.trim())?;
            }
            writeln!(output)?;
            for (index, option) in question.options.iter().enumerate() {
                writeln!(output, "  {}) {}", index + 1, option)?;
            }
            writeln!(
                output,
                "  {}) Other (type your own answer)",
                question.options.len() + 1
            )?;
            output.flush()?;
            Ok(())
        })?;

        loop {
            let selection = prompt_line(&format!(
                "Enter number (1-{}): ",
                question.options.len() + 1
            ))?;
            let Ok(selected) = selection.trim().parse::<usize>() else {
                prompt_error("invalid selection, enter a number")?;
                continue;
            };
            if selected == 0 || selected > question.options.len() + 1 {
                prompt_error(&format!(
                    "invalid selection, enter a number between 1 and {}",
                    question.options.len() + 1
                ))?;
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
        with_prompt_output(|output, output_is_terminal| {
            writeln!(output)?;
            writeln!(output, "Plan draft")?;
            writeln!(output, "Target: {}", draft.target_path)?;
            writeln!(output, "--------------------")?;
            render_markdown_draft(output, &draft.draft, self.theme, output_is_terminal)?;
            writeln!(output, "--------------------")?;
            writeln!(output)?;
            writeln!(output, "  1) Accept")?;
            writeln!(output, "  2) Revise")?;
            writeln!(output, "  3) Reject")?;
            output.flush()?;
            Ok(())
        })?;

        loop {
            match prompt_line("Enter number (1-3): ")?.trim() {
                "1" => {
                    return Ok(PlanningDraftDecision {
                        kind: PlanningDraftDecisionKind::Accept,
                        feedback: None,
                    });
                }
                "2" => {
                    return Ok(PlanningDraftDecision {
                        kind: PlanningDraftDecisionKind::Revise,
                        feedback: Some(prompt_multiline_nonempty("Enter revision feedback:")?),
                    });
                }
                "3" => {
                    return Ok(PlanningDraftDecision {
                        kind: PlanningDraftDecisionKind::Reject,
                        feedback: None,
                    });
                }
                _ => {
                    prompt_error("invalid selection, enter 1, 2, or 3")?;
                }
            }
        }
    }
}

pub fn edit_file(path: &Utf8Path, editor_override: Option<&str>) -> Result<()> {
    let visual = env::var("VISUAL").ok();
    let editor = env::var("EDITOR").ok();
    let editor = require_editor_command(editor_override, visual.as_deref(), editor.as_deref())?;
    edit_file_with_external_editor(path, &editor)
}

pub fn prompt_nonempty(prompt: &str) -> Result<String> {
    with_prompt_io(|input, output, _| prompt_nonempty_with_io(prompt, input, output))
}

pub fn prompt_multiline_nonempty(prompt: &str) -> Result<String> {
    with_prompt_io(|input, output, _| prompt_multiline_nonempty_with_io(prompt, input, output))
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
            _ => prompt_error("enter y or n")?,
        }
    }
}

pub fn prompt_line(prompt: &str) -> Result<String> {
    with_prompt_io(|input, output, _| prompt_line_with_io(prompt, input, output))
}

fn render_markdown_draft(
    output: &mut (impl Write + ?Sized),
    markdown: &str,
    theme: TerminalTheme,
    output_is_terminal: bool,
) -> Result<()> {
    if output_is_terminal {
        write!(output, "{}", planning_draft_skin(theme).term_text(markdown))?;
        if !markdown.ends_with('\n') {
            writeln!(output)?;
        }
        return Ok(());
    }

    writeln!(output, "{markdown}")?;
    if !markdown.ends_with('\n') {
        writeln!(output)?;
    }
    Ok(())
}

fn planning_draft_skin(theme: TerminalTheme) -> MadSkin {
    let palette = theme.palette();
    let mut skin = if theme.colors_enabled() {
        match palette.variant {
            ralph_core::ThemeVariant::Dark => MadSkin::default_dark(),
            ralph_core::ThemeVariant::Light => MadSkin::default_light(),
        }
    } else {
        MadSkin::no_style()
    };

    skin.set_fg(termimad_color(palette.text));
    skin.set_headers_fg(termimad_color(palette.accent));
    skin.bold.set_fg(termimad_color(palette.text));
    skin.italic.set_fg(termimad_color(palette.muted));
    skin.inline_code.set_fg(termimad_color(palette.accent));
    skin.code_block.set_fg(termimad_color(palette.text));
    skin.bullet.set_fg(termimad_color(palette.accent));
    skin.quote_mark.set_fg(termimad_color(palette.subtle));
    skin.horizontal_rule.set_fg(termimad_color(palette.subtle));
    skin
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

fn require_editor_command(
    editor_override: Option<&str>,
    visual: Option<&str>,
    editor: Option<&str>,
) -> Result<String> {
    resolve_editor_command(editor_override, visual, editor).ok_or_else(|| {
        anyhow!("no editor configured; set project `editor_override`, `VISUAL`, or `EDITOR`")
    })
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

fn prompt_nonempty_with_io(
    prompt: &str,
    input: &mut (impl BufRead + ?Sized),
    output: &mut (impl Write + ?Sized),
) -> Result<String> {
    loop {
        let line = prompt_line_with_io(prompt, input, output)?;
        if !line.trim().is_empty() {
            return Ok(line);
        }
        writeln!(output, "input cannot be empty")?;
        output.flush()?;
    }
}

fn prompt_multiline_nonempty_with_io(
    prompt: &str,
    input: &mut (impl BufRead + ?Sized),
    output: &mut (impl Write + ?Sized),
) -> Result<String> {
    loop {
        writeln!(output, "{prompt}")?;
        writeln!(output, "Finish with a single '.' on its own line.")?;
        output.flush()?;

        let mut lines = Vec::new();
        loop {
            let line = prompt_line_with_io("> ", input, output)?;
            if line.trim() == "." {
                break;
            }
            lines.push(line);
        }

        let body = lines.join("\n");
        if !body.trim().is_empty() {
            return Ok(body);
        }

        writeln!(output, "input cannot be empty")?;
        output.flush()?;
    }
}

fn prompt_line_with_io(
    prompt: &str,
    input: &mut (impl BufRead + ?Sized),
    output: &mut (impl Write + ?Sized),
) -> Result<String> {
    write!(output, "{prompt}")?;
    output.flush()?;

    let mut line = String::new();
    let bytes_read = input.read_line(&mut line)?;
    if bytes_read == 0 {
        return Err(anyhow!("interactive input closed"));
    }

    Ok(line.trim_end_matches(['\r', '\n']).to_owned())
}

fn prompt_error(message: &str) -> Result<()> {
    with_prompt_output(|output, _| {
        writeln!(output, "{message}")?;
        output.flush()?;
        Ok(())
    })
}

fn with_prompt_io<T>(
    operation: impl FnOnce(&mut dyn BufRead, &mut dyn Write, bool) -> Result<T>,
) -> Result<T> {
    if let Some((mut input, mut output)) = open_console_io() {
        return operation(&mut input, &mut output, true);
    }

    let stdin = io::stdin();
    let stderr = io::stderr();
    let stderr_is_terminal = stderr.is_terminal();
    let mut input = stdin.lock();
    let mut output = stderr.lock();
    operation(&mut input, &mut output, stderr_is_terminal)
}

fn with_prompt_output<T>(operation: impl FnOnce(&mut dyn Write, bool) -> Result<T>) -> Result<T> {
    if let Some(mut output) = open_console_output() {
        return operation(&mut output, true);
    }

    let stderr = io::stderr();
    let stderr_is_terminal = stderr.is_terminal();
    let mut output = stderr.lock();
    operation(&mut output, stderr_is_terminal)
}

fn open_console_io() -> Option<(io::BufReader<File>, File)> {
    let input = File::open(CONSOLE_INPUT_PATH).ok()?;
    let output = OpenOptions::new()
        .write(true)
        .open(CONSOLE_OUTPUT_PATH)
        .ok()?;
    Some((io::BufReader::new(input), output))
}

fn open_console_output() -> Option<File> {
    OpenOptions::new()
        .write(true)
        .open(CONSOLE_OUTPUT_PATH)
        .ok()
}

fn termimad_color(color: ThemeColor) -> Color {
    match color {
        ThemeColor::Black => Color::Black,
        ThemeColor::Red => Color::DarkRed,
        ThemeColor::Green => Color::DarkGreen,
        ThemeColor::Yellow => Color::DarkYellow,
        ThemeColor::Blue => Color::DarkBlue,
        ThemeColor::Magenta => Color::DarkMagenta,
        ThemeColor::Cyan => Color::DarkCyan,
        ThemeColor::Gray => Color::Grey,
        ThemeColor::DarkGray => Color::DarkGrey,
        ThemeColor::LightRed => Color::Red,
        ThemeColor::LightGreen => Color::Green,
        ThemeColor::LightYellow => Color::Yellow,
        ThemeColor::LightBlue => Color::Blue,
        ThemeColor::LightMagenta => Color::Magenta,
        ThemeColor::LightCyan => Color::Cyan,
        ThemeColor::White => Color::White,
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
    use std::{fs, io::Cursor};

    use camino::Utf8PathBuf;

    use super::{
        edit_file_with_external_editor, prompt_line_with_io, prompt_multiline_nonempty_with_io,
        require_editor_command, resolve_editor_command,
    };

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

    #[test]
    fn missing_editor_configuration_returns_error() {
        let error = require_editor_command(None, None, None)
            .unwrap_err()
            .to_string();
        assert!(error.contains("no editor configured"));
    }

    #[test]
    fn prompt_line_errors_when_input_is_closed() {
        let mut input = Cursor::new(Vec::<u8>::new());
        let mut output = Vec::new();
        let error = prompt_line_with_io("Prompt: ", &mut input, &mut output)
            .unwrap_err()
            .to_string();
        assert!(error.contains("interactive input closed"));
    }

    #[test]
    fn multiline_feedback_preserves_newlines() {
        let mut input = Cursor::new(b"first line\nsecond line\n.\n".to_vec());
        let mut output = Vec::new();
        let feedback =
            prompt_multiline_nonempty_with_io("Enter revision feedback:", &mut input, &mut output)
                .unwrap();
        assert_eq!(feedback, "first line\nsecond line");
    }

    #[test]
    fn editor_command_reports_non_zero_exit_status() {
        let temp = tempfile::tempdir().unwrap();
        let path = Utf8PathBuf::from_path_buf(temp.path().join("plan.md")).unwrap();
        fs::write(path.as_std_path(), "# draft\n").unwrap();

        let error = edit_file_with_external_editor(&path, "false")
            .unwrap_err()
            .to_string();
        assert!(error.contains("exited with status"));
    }

    #[test]
    fn editor_command_quotes_paths_with_spaces() {
        let temp = tempfile::tempdir().unwrap();
        let dir = Utf8PathBuf::from_path_buf(temp.path().join("dir with spaces")).unwrap();
        fs::create_dir_all(dir.as_std_path()).unwrap();
        let path = dir.join("plan file.md");
        let captured = temp.path().join("captured.txt");
        fs::write(path.as_std_path(), "# draft\n").unwrap();

        let command = format!(
            "sh -c {} --",
            super::quote_shell_arg(&format!(
                "printf '%s' \"$1\" > {}",
                super::quote_shell_arg(captured.to_str().unwrap())
            ))
        );

        edit_file_with_external_editor(&path, &command).unwrap();
        assert_eq!(fs::read_to_string(captured).unwrap(), path.as_str());
    }
}
