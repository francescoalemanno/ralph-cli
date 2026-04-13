use std::{
    env,
    fs::{self, File, OpenOptions},
    io::{self, BufRead, IsTerminal, Write},
    process::Command,
};

use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use camino::{Utf8Path, Utf8PathBuf};
use ralph_core::{LastRunStatus, TerminalTheme, ThemeColor, ThemeConfig};
use serde_json::Value;
use similar::TextDiff;
use termimad::{MadSkin, crossterm::style::Color};
use time::{OffsetDateTime, format_description::FormatItem, macros::format_description};

use crate::{
    PlanningAnswerSource, PlanningDraftDecision, PlanningDraftDecisionKind, PlanningDraftReview,
    PlanningQuestion, PlanningQuestionAnswer, RunDelegate, RunEvent,
};

const OUTPUT_TIMESTAMP_FORMAT: &[FormatItem<'static>] =
    format_description!("[hour]:[minute]:[second]");

#[derive(Debug, Clone)]
pub struct ConsoleDelegate {
    theme: TerminalTheme,
    output_buffer: ConsoleOutputBuffer,
    editor_override: Option<String>,
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
        Self::new_with_editor_override(theme_config, None)
    }

    pub fn new_with_editor_override(
        theme_config: &ThemeConfig,
        editor_override: Option<&str>,
    ) -> Self {
        Self {
            theme: TerminalTheme::new(theme_config),
            output_buffer: ConsoleOutputBuffer::default(),
            editor_override: editor_override.map(str::to_owned),
        }
    }

    fn render_output_chunk(&mut self, chunk: &str) -> Result<()> {
        with_stdout(|output| {
            self.output_buffer
                .push_chunk(chunk, output, current_output_timestamp)
        })
    }

    fn flush_pending_output(&mut self) -> Result<()> {
        with_stdout(|output| self.output_buffer.flush(output, current_output_timestamp))
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
                self.flush_pending_output()?;
                println!();
                println!(
                    "{}",
                    format_iteration_banner(self.theme, &prompt_name, iteration, max_iterations)
                );
            }
            RunEvent::Output(chunk) => {
                self.render_output_chunk(&chunk)?;
            }
            RunEvent::ParallelWorkerLaunched { channel_id, label } => {
                self.flush_pending_output()?;
                println!(
                    "{}",
                    format_parallel_event(self.theme, "queued", &channel_id, &label, None)
                );
            }
            RunEvent::ParallelWorkerStarted { channel_id, label } => {
                self.flush_pending_output()?;
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
                self.flush_pending_output()?;
                println!(
                    "{}",
                    format_parallel_event(self.theme, "done", &channel_id, &label, Some(exit_code))
                );
            }
            RunEvent::Note(note) => {
                self.flush_pending_output()?;
                eprintln!("{}", format_note(self.theme, &note));
            }
            RunEvent::Finished { status, summary } => {
                self.flush_pending_output()?;
                println!("\n{}", format_finish_line(self.theme, status, &summary));
            }
        }
        Ok(())
    }

    async fn answer_planning_question(
        &mut self,
        question: &PlanningQuestion,
    ) -> Result<PlanningQuestionAnswer> {
        self.flush_pending_output()?;
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
        self.flush_pending_output()?;
        with_prompt_io(|input, output, output_is_terminal| {
            self.review_planning_draft_with_io(draft, input, output, output_is_terminal, edit_file)
        })
    }
}

impl ConsoleDelegate {
    fn review_planning_draft_with_io<F>(
        &mut self,
        draft: &PlanningDraftReview,
        input: &mut (impl BufRead + ?Sized),
        output: &mut (impl Write + ?Sized),
        output_is_terminal: bool,
        mut edit_with_editor: F,
    ) -> Result<PlanningDraftDecision>
    where
        F: FnMut(&Utf8Path, Option<&str>) -> Result<()>,
    {
        render_planning_draft_review(
            output,
            &draft.draft,
            &draft.target_path,
            self.theme,
            output_is_terminal,
        )?;

        loop {
            match prompt_line_with_io("Enter number (1-4): ", input, output)?.trim() {
                "1" => {
                    return Ok(PlanningDraftDecision {
                        kind: PlanningDraftDecisionKind::Accept,
                        feedback: None,
                    });
                }
                "2" => {
                    return Ok(PlanningDraftDecision {
                        kind: PlanningDraftDecisionKind::Revise,
                        feedback: Some(prompt_multiline_nonempty_with_io(
                            "Enter revision feedback:",
                            input,
                            output,
                        )?),
                    });
                }
                "3" => match self.review_planning_draft_with_external_editor(
                    draft,
                    output,
                    &mut edit_with_editor,
                ) {
                    Ok(Some(decision)) => return Ok(decision),
                    Ok(None) => {}
                    Err(error) => {
                        prompt_error_with_io(output, &error.to_string())?;
                    }
                },
                "4" => {
                    return Ok(PlanningDraftDecision {
                        kind: PlanningDraftDecisionKind::Reject,
                        feedback: None,
                    });
                }
                _ => {
                    prompt_error_with_io(output, "invalid selection, enter 1, 2, 3, or 4")?;
                }
            }
        }
    }

    fn review_planning_draft_with_external_editor<F>(
        &self,
        draft: &PlanningDraftReview,
        output: &mut (impl Write + ?Sized),
        edit_with_editor: &mut F,
    ) -> Result<Option<PlanningDraftDecision>>
    where
        F: FnMut(&Utf8Path, Option<&str>) -> Result<()>,
    {
        let revised_draft = edit_planning_draft_in_temp_copy(
            draft,
            self.editor_override.as_deref(),
            edit_with_editor,
        )?;
        if revised_draft == draft.draft {
            prompt_error_with_io(
                output,
                "No external changes detected; draft review remains unchanged.",
            )?;
            return Ok(None);
        }

        let udiff = render_external_review_diff(&draft.draft, &revised_draft);
        write_external_review_diff(output, &udiff)?;
        Ok(Some(PlanningDraftDecision {
            kind: PlanningDraftDecisionKind::Revise,
            feedback: Some(format_external_edit_review_feedback(&udiff)),
        }))
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct ConsoleOutputBuffer {
    pending: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ConsoleRenderedLine {
    Raw(String),
    ExtractedTexts(Vec<String>),
    Suppressed,
}

impl ConsoleOutputBuffer {
    fn push_chunk<F>(
        &mut self,
        chunk: &str,
        output: &mut (impl Write + ?Sized),
        mut now: F,
    ) -> Result<()>
    where
        F: FnMut() -> String,
    {
        if chunk.is_empty() {
            return Ok(());
        }

        self.pending.push_str(chunk);
        while let Some(newline_index) = self.pending.find('\n') {
            let remainder = self.pending.split_off(newline_index + 1);
            let line = std::mem::replace(&mut self.pending, remainder);
            write_rendered_line(
                render_console_line(trim_trailing_line_endings(&line)),
                output,
                &mut now,
            )?;
            output.flush()?;
        }

        Ok(())
    }

    fn flush<F>(&mut self, output: &mut (impl Write + ?Sized), mut now: F) -> Result<()>
    where
        F: FnMut() -> String,
    {
        if self.pending.is_empty() {
            return Ok(());
        }

        let line = std::mem::take(&mut self.pending);
        write_rendered_line(
            render_console_line(trim_trailing_line_endings(&line)),
            output,
            &mut now,
        )?;
        output.flush()?;
        Ok(())
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

fn render_planning_draft_review(
    output: &mut (impl Write + ?Sized),
    draft: &str,
    target_path: &Utf8Path,
    theme: TerminalTheme,
    output_is_terminal: bool,
) -> Result<()> {
    writeln!(output)?;
    writeln!(output, "Plan draft")?;
    writeln!(output, "Target: {}", target_path)?;
    writeln!(output, "--------------------")?;
    render_markdown_draft(output, draft, theme, output_is_terminal)?;
    writeln!(output, "--------------------")?;
    writeln!(output)?;
    writeln!(output, "  1) Accept")?;
    writeln!(output, "  2) Revise")?;
    writeln!(output, "  3) Edit externally")?;
    writeln!(output, "  4) Reject")?;
    output.flush()?;
    Ok(())
}

fn edit_planning_draft_in_temp_copy<F>(
    draft: &PlanningDraftReview,
    editor_override: Option<&str>,
    edit_with_editor: &mut F,
) -> Result<String>
where
    F: FnMut(&Utf8Path, Option<&str>) -> Result<()>,
{
    let temp =
        tempfile::tempdir().context("failed to create temporary planning draft directory")?;
    let temp_path = planning_temp_copy_path(
        &temp
            .path()
            .join(draft.target_path.file_name().unwrap_or("plan-draft.md")),
    )?;
    fs::write(temp_path.as_std_path(), &draft.draft)
        .with_context(|| format!("failed to write temporary planning draft {}", temp_path))?;
    edit_with_editor(&temp_path, editor_override)?;
    fs::read_to_string(temp_path.as_std_path())
        .with_context(|| format!("failed to read revised planning draft {}", temp_path))
}

fn planning_temp_copy_path(path: &std::path::Path) -> Result<Utf8PathBuf> {
    Utf8PathBuf::from_path_buf(path.to_path_buf())
        .map_err(|_| anyhow!("temporary planning draft path is not valid UTF-8"))
}

fn render_external_review_diff(old_text: &str, new_text: &str) -> String {
    format!(
        "{}",
        TextDiff::from_lines(old_text, new_text)
            .unified_diff()
            .context_radius(4)
            .header("Old plan draft", "User revised draft")
    )
}

fn format_external_edit_review_feedback(udiff: &str) -> String {
    format!(
        "Review source: external edit\n\nApply this unified diff to the latest draft:\n\n{}",
        udiff.trim_end()
    )
}

fn write_external_review_diff(output: &mut (impl Write + ?Sized), udiff: &str) -> Result<()> {
    writeln!(output)?;
    writeln!(output, "External review diff")?;
    writeln!(output, "--------------------")?;
    write!(output, "{udiff}")?;
    if !udiff.ends_with('\n') {
        writeln!(output)?;
    }
    writeln!(output, "--------------------")?;
    output.flush()?;
    Ok(())
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
    with_prompt_output(|output, _| prompt_error_with_io(output, message))
}

fn prompt_error_with_io(output: &mut (impl Write + ?Sized), message: &str) -> Result<()> {
    writeln!(output, "{message}")?;
    output.flush()?;
    Ok(())
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

fn with_stdout<T>(operation: impl FnOnce(&mut dyn Write) -> Result<T>) -> Result<T> {
    let stdout = io::stdout();
    let mut output = stdout.lock();
    let result = operation(&mut output)?;
    output.flush()?;
    Ok(result)
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

fn trim_trailing_line_endings(line: &str) -> &str {
    line.trim_end_matches(['\r', '\n'])
}

fn render_console_line(line: &str) -> ConsoleRenderedLine {
    match serde_json::from_str::<Value>(line) {
        Ok(value) => {
            let mut texts = Vec::new();
            collect_json_texts(&value, &mut texts);
            if texts.is_empty() {
                ConsoleRenderedLine::Suppressed
            } else {
                ConsoleRenderedLine::ExtractedTexts(texts)
            }
        }
        Err(_) => ConsoleRenderedLine::Raw(line.to_owned()),
    }
}

fn collect_json_texts(value: &Value, texts: &mut Vec<String>) {
    match value {
        Value::Object(map) => {
            if let Some(text) = map.get("text").and_then(json_text_value) {
                texts.push(text);
            }
            for value in map.values() {
                collect_json_texts(value, texts);
            }
        }
        Value::Array(values) => {
            for value in values {
                collect_json_texts(value, texts);
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {}
    }
}

fn json_text_value(value: &Value) -> Option<String> {
    match value {
        Value::Null => None,
        Value::Bool(value) => Some(value.to_string()),
        Value::Number(value) => Some(value.to_string()),
        Value::String(value) => Some(value.clone()),
        Value::Array(_) | Value::Object(_) => None,
    }
}

fn write_rendered_line<F>(
    line: ConsoleRenderedLine,
    output: &mut (impl Write + ?Sized),
    now: &mut F,
) -> Result<()>
where
    F: FnMut() -> String,
{
    match line {
        ConsoleRenderedLine::Raw(line) => {
            writeln!(output, "{line}")?;
        }
        ConsoleRenderedLine::ExtractedTexts(texts) => {
            let timestamp = now();
            for text in texts {
                writeln!(output, "[{timestamp}] {text}")?;
            }
        }
        ConsoleRenderedLine::Suppressed => {}
    }
    Ok(())
}

fn current_output_timestamp() -> String {
    OffsetDateTime::now_local()
        .unwrap_or_else(|_| OffsetDateTime::now_utc())
        .format(OUTPUT_TIMESTAMP_FORMAT)
        .unwrap_or_else(|_| "00:00:00".to_owned())
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
    use std::{fs, io::Cursor, io::Write};

    use anyhow::anyhow;
    use camino::Utf8PathBuf;
    use ralph_core::ThemeConfig;

    use super::{
        ConsoleDelegate, ConsoleOutputBuffer, ConsoleRenderedLine, edit_file_with_external_editor,
        prompt_line_with_io, prompt_multiline_nonempty_with_io, render_console_line,
        render_external_review_diff, require_editor_command, resolve_editor_command,
    };
    use crate::{PlanningDraftDecisionKind, PlanningDraftReview};

    #[derive(Default)]
    struct FlushTrackingWriter {
        bytes: Vec<u8>,
        flush_count: usize,
    }

    impl Write for FlushTrackingWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.bytes.extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            self.flush_count += 1;
            Ok(())
        }
    }

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

    #[test]
    fn external_edit_review_returns_revise_with_unified_diff_feedback() {
        let mut delegate =
            ConsoleDelegate::new_with_editor_override(&ThemeConfig::default(), Some("code -w"));
        let draft = PlanningDraftReview {
            target_path: Utf8PathBuf::from("docs/plans/cache-plan.md"),
            draft: "# Cache Plan\n\n## Overview\nInitial draft.\n".to_owned(),
        };
        let mut input = Cursor::new(b"3\n".to_vec());
        let mut output = Vec::new();

        let decision = delegate
            .review_planning_draft_with_io(
                &draft,
                &mut input,
                &mut output,
                false,
                |path, editor_override| {
                    assert_eq!(editor_override, Some("code -w"));
                    assert_eq!(fs::read_to_string(path.as_std_path()).unwrap(), draft.draft);
                    fs::write(
                        path.as_std_path(),
                        "# Cache Plan\n\n## Overview\nUser revised draft.\n",
                    )
                    .unwrap();
                    Ok(())
                },
            )
            .unwrap();

        assert_eq!(decision.kind, PlanningDraftDecisionKind::Revise);
        let feedback = decision.feedback.unwrap();
        assert!(feedback.contains("Review source: external edit"));
        assert!(feedback.contains("--- Old plan draft"));
        assert!(feedback.contains("+++ User revised draft"));
        assert!(feedback.contains("User revised draft."));

        let rendered = String::from_utf8(output).unwrap();
        assert!(rendered.contains("  3) Edit externally"));
        assert!(rendered.contains("External review diff"));
        assert!(rendered.contains("--- Old plan draft"));
        assert!(rendered.contains("+++ User revised draft"));
    }

    #[test]
    fn external_edit_review_noop_returns_to_menu() {
        let mut delegate = ConsoleDelegate::new(&ThemeConfig::default());
        let draft = PlanningDraftReview {
            target_path: Utf8PathBuf::from("docs/plans/cache-plan.md"),
            draft: "# Cache Plan\n".to_owned(),
        };
        let mut input = Cursor::new(b"3\n4\n".to_vec());
        let mut output = Vec::new();

        let decision = delegate
            .review_planning_draft_with_io(&draft, &mut input, &mut output, false, |_, _| Ok(()))
            .unwrap();

        assert_eq!(decision.kind, PlanningDraftDecisionKind::Reject);
        assert!(decision.feedback.is_none());
        assert!(
            String::from_utf8(output)
                .unwrap()
                .contains("No external changes detected")
        );
    }

    #[test]
    fn external_edit_review_editor_error_returns_to_menu() {
        let mut delegate = ConsoleDelegate::new(&ThemeConfig::default());
        let draft = PlanningDraftReview {
            target_path: Utf8PathBuf::from("docs/plans/cache-plan.md"),
            draft: "# Cache Plan\n".to_owned(),
        };
        let mut input = Cursor::new(b"3\n4\n".to_vec());
        let mut output = Vec::new();

        let decision = delegate
            .review_planning_draft_with_io(&draft, &mut input, &mut output, false, |_, _| {
                Err(anyhow!("editor launch failed"))
            })
            .unwrap();

        assert_eq!(decision.kind, PlanningDraftDecisionKind::Reject);
        assert!(decision.feedback.is_none());
        assert!(
            String::from_utf8(output)
                .unwrap()
                .contains("editor launch failed")
        );
    }

    #[test]
    fn external_review_diff_uses_expected_headers() {
        let udiff = render_external_review_diff("alpha\nbeta\ngamma\n", "alpha\nbeta\nrevised\n");

        assert!(udiff.contains("--- Old plan draft"));
        assert!(udiff.contains("+++ User revised draft"));
        assert!(udiff.contains("@@"));
        assert!(udiff.contains("-gamma"));
        assert!(udiff.contains("+revised"));
    }

    #[test]
    fn output_buffer_renders_raw_lines_line_by_line() {
        let mut buffer = ConsoleOutputBuffer::default();
        let mut output = Vec::new();
        buffer
            .push_chunk("alpha\nbeta\n", &mut output, || "12:34:56".to_owned())
            .unwrap();
        assert_eq!(String::from_utf8(output).unwrap(), "alpha\nbeta\n");
    }

    #[test]
    fn output_buffer_waits_for_complete_line_before_rendering() {
        let mut buffer = ConsoleOutputBuffer::default();
        let mut output = Vec::new();
        buffer
            .push_chunk("al", &mut output, || "12:34:56".to_owned())
            .unwrap();
        assert!(output.is_empty());

        buffer
            .push_chunk("pha\nbe", &mut output, || "12:34:56".to_owned())
            .unwrap();
        assert_eq!(String::from_utf8(output.clone()).unwrap(), "alpha\n");

        buffer.flush(&mut output, || "12:34:56".to_owned()).unwrap();
        assert_eq!(String::from_utf8(output).unwrap(), "alpha\nbe\n");
    }

    #[test]
    fn output_buffer_flushes_immediately_after_completed_lines() {
        let mut buffer = ConsoleOutputBuffer::default();
        let mut output = FlushTrackingWriter::default();
        buffer
            .push_chunk("alpha\nbeta\n", &mut output, || "12:34:56".to_owned())
            .unwrap();

        assert_eq!(String::from_utf8(output.bytes).unwrap(), "alpha\nbeta\n");
        assert_eq!(output.flush_count, 2);
    }

    #[test]
    fn output_buffer_extracts_nested_json_text_fields() {
        let mut buffer = ConsoleOutputBuffer::default();
        let mut output = Vec::new();
        buffer
            .push_chunk(
                r#"{"delta":{"text":"hello"},"items":[{"ignored":1},{"text":"world"}]}"#,
                &mut output,
                || "12:34:56".to_owned(),
            )
            .unwrap();
        buffer
            .push_chunk("\n", &mut output, || "12:34:56".to_owned())
            .unwrap();

        assert_eq!(
            String::from_utf8(output).unwrap(),
            "[12:34:56] hello\n[12:34:56] world\n"
        );
    }

    #[test]
    fn output_buffer_suppresses_json_lines_without_text_fields() {
        let mut buffer = ConsoleOutputBuffer::default();
        let mut output = Vec::new();
        buffer
            .push_chunk(
                r#"{"type":"status","phase":"running"}\n"#,
                &mut output,
                || "12:34:56".to_owned(),
            )
            .unwrap();
        assert!(String::from_utf8(output).unwrap().is_empty());
    }

    #[test]
    fn console_line_without_json_text_is_suppressed() {
        assert_eq!(
            render_console_line(r#"{"type":"status","phase":"running"}"#),
            ConsoleRenderedLine::Suppressed
        );
    }
}
