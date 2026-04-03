use std::{borrow::Cow, env, io, io::BufRead, path::PathBuf, process::Command};

use anyhow::{Context, Result, anyhow};
use camino::Utf8Path;
use crossterm::{
    event::{self, DisableMouseCapture, EnableMouseCapture, Event as CEvent, KeyEventKind},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ralph_core::atomic_write;
use ratatui::{
    Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    widgets::{Block, BorderType, Borders, Paragraph},
};
use tui_textarea::{Input, Key, TextArea};

use crate::ui::styled_title;

pub fn edit_file(path: &Utf8Path, editor_override: Option<&str>) -> Result<()> {
    if let Some(editor) = preferred_external_editor(editor_override) {
        return edit_file_with_external_editor(path, &editor);
    }

    PromptEditor::new(path.as_std_path().to_path_buf())?.run()?;
    Ok(())
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

struct PromptEditor<'a> {
    path: PathBuf,
    textarea: TextArea<'a>,
    terminal: Terminal<CrosstermBackend<io::Stdout>>,
    modified: bool,
    message: Option<Cow<'static, str>>,
}

impl<'a> PromptEditor<'a> {
    fn new(path: PathBuf) -> Result<Self> {
        let mut stdout = io::stdout();
        enable_raw_mode().context("failed to enable raw mode for internal editor")?;
        execute!(stdout, EnterAlternateScreen, EnableMouseCapture)
            .context("failed to enter alternate screen for internal editor")?;
        let backend = CrosstermBackend::new(stdout);
        let terminal = Terminal::new(backend).context("failed to create editor terminal")?;

        let mut textarea = load_text_area(&path)?;
        textarea.set_block(
            Block::default()
                .title(styled_title(
                    "Prompt Editor",
                    "Ctrl-S saves  ◆  Ctrl-Q closes",
                    Color::White,
                    Color::DarkGray,
                    Color::Gray,
                ))
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded),
        );
        textarea.set_line_number_style(Style::default().fg(Color::DarkGray));

        Ok(Self {
            path,
            textarea,
            terminal,
            modified: false,
            message: None,
        })
    }

    fn run(&mut self) -> Result<()> {
        loop {
            let path = self.path.display().to_string();
            let modified = self.modified;
            let message = self.message.take();
            let textarea = &self.textarea;
            self.terminal.draw(|frame| {
                let (footer_text, footer_height) = if let Some(message) = message.as_ref() {
                    (Cow::Borrowed(message.as_ref()), 1)
                } else {
                    editor_help_text(frame.area().width)
                };
                let layout = Layout::default()
                    .direction(Direction::Vertical)
                    .constraints([
                        Constraint::Min(1),
                        Constraint::Length(1),
                        Constraint::Length(footer_height),
                    ])
                    .split(frame.area());

                frame.render_widget(textarea, layout[0]);

                let modified_suffix = if modified { " [modified]" } else { "" };
                let path = format!(" {}{} ", path, modified_suffix);
                let (row, col) = textarea.cursor();
                let cursor = format!("({},{})", row + 1, col + 1);
                let status_chunks = Layout::default()
                    .direction(Direction::Horizontal)
                    .constraints([Constraint::Min(1), Constraint::Length(cursor.len() as u16)])
                    .split(layout[1]);
                let status_style = Style::default().add_modifier(Modifier::REVERSED);
                frame.render_widget(Paragraph::new(path).style(status_style), status_chunks[0]);
                frame.render_widget(Paragraph::new(cursor).style(status_style), status_chunks[1]);
                frame.render_widget(
                    Paragraph::new(footer_text.as_ref()).style(Style::default().fg(Color::Gray)),
                    layout[2],
                );
            })?;

            match event::read().context("failed while reading editor input")? {
                CEvent::Key(key) if key.kind == KeyEventKind::Press => {
                    let input: Input = key.into();
                    match input {
                        Input {
                            key: Key::Char('s'),
                            ctrl: true,
                            ..
                        } => {
                            self.save()?;
                            self.message = Some("Saved".into());
                        }
                        Input {
                            key: Key::Char('q'),
                            ctrl: true,
                            ..
                        } => break,
                        input => {
                            self.modified |= self.textarea.input(input);
                        }
                    }
                }
                CEvent::Mouse(_) | CEvent::Resize(_, _) => {}
                _ => {}
            }
        }

        Ok(())
    }

    fn save(&mut self) -> Result<()> {
        let mut contents = self.textarea.lines().join("\n");
        contents.push('\n');
        atomic_write(&self.path, contents)
            .with_context(|| format!("failed to save {}", self.path.display()))?;
        self.modified = false;
        Ok(())
    }
}

impl Drop for PromptEditor<'_> {
    fn drop(&mut self) {
        self.terminal.show_cursor().ok();
        disable_raw_mode().ok();
        execute!(
            self.terminal.backend_mut(),
            LeaveAlternateScreen,
            DisableMouseCapture
        )
        .ok();
    }
}

fn load_text_area<'a>(path: &PathBuf) -> Result<TextArea<'a>> {
    if let Ok(metadata) = path.metadata() {
        if !metadata.is_file() {
            return Err(anyhow!("{} is not a file", path.display()));
        }

        let mut textarea: TextArea<'a> = io::BufReader::new(
            std::fs::File::open(path)
                .with_context(|| format!("failed to open {}", path.display()))?,
        )
        .lines()
        .collect::<io::Result<_>>()
        .with_context(|| format!("failed to read {}", path.display()))?;
        if textarea.lines().iter().any(|line| line.starts_with('\t')) {
            textarea.set_hard_tab_indent(true);
        }
        Ok(textarea)
    } else {
        Ok(TextArea::default())
    }
}

fn editor_help_text(width: u16) -> (Cow<'static, str>, u16) {
    const SINGLE_LINE: &str = "^A/^E line ends  M-B/M-F word jump  ^W/M-D delete word  ^U/^R undo redo  ^V/M-V page scroll  ^S save  ^Q close";
    const TWO_LINES: &str = "^A/^E line ends  M-B/M-F word jump  ^W/M-D delete word\n^U/^R undo redo  ^V/M-V page scroll  ^S save  ^Q close";

    if width as usize >= SINGLE_LINE.len() {
        (Cow::Borrowed(SINGLE_LINE), 1)
    } else {
        (Cow::Borrowed(TWO_LINES), 2)
    }
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
    fn visual_then_editor_are_used_when_override_is_missing_or_blank() {
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
