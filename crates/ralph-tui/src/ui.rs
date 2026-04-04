use std::io;

use anyhow::{Context, Result};
use crossterm::{
    event::{DisableMouseCapture, EnableMouseCapture},
    execute,
    terminal::{
        Clear as TerminalClear, ClearType, EnterAlternateScreen, LeaveAlternateScreen,
        disable_raw_mode, enable_raw_mode,
    },
};
use ralph_core::LastRunStatus;
use ratatui::{
    Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
};

pub(crate) fn suspend_terminal(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
) -> Result<()> {
    disable_raw_mode().ok();
    execute!(
        terminal.backend_mut(),
        DisableMouseCapture,
        LeaveAlternateScreen
    )
    .context("failed to leave alternate screen")?;
    Ok(())
}

pub(crate) fn resume_terminal(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>) -> Result<()> {
    execute!(
        terminal.backend_mut(),
        EnterAlternateScreen,
        EnableMouseCapture
    )
    .context("failed to re-enter alternate screen")?;
    enable_raw_mode().context("failed to re-enable raw mode")?;
    execute!(terminal.backend_mut(), TerminalClear(ClearType::All))
        .context("failed to clear terminal after editor exit")?;
    terminal
        .clear()
        .context("failed to reset terminal buffer after editor exit")?;
    terminal.hide_cursor().ok();
    Ok(())
}

pub(crate) fn centered_rect(percent_x: u16, percent_y: u16, area: Rect) -> Rect {
    let popup = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(area);

    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(popup[1])[1]
}

pub(crate) fn styled_title(
    title: &str,
    subtitle: &str,
    text_color: Color,
    subtle_color: Color,
    muted_color: Color,
) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            format!(" {} ", title),
            Style::default().fg(text_color).add_modifier(Modifier::BOLD),
        ),
        Span::styled("◆", Style::default().fg(subtle_color)),
        Span::styled(format!(" {}", subtitle), Style::default().fg(muted_color)),
    ])
}

pub(crate) fn key_style(color: Color) -> Style {
    Style::default().fg(color).add_modifier(Modifier::BOLD)
}

pub(crate) fn normalize_terminal_text(text: &str) -> Vec<u8> {
    let mut normalized = Vec::with_capacity(text.len() + 16);
    let mut previous = None;
    for byte in text.bytes() {
        if byte == b'\n' && previous != Some(b'\r') {
            normalized.push(b'\r');
        }
        normalized.push(byte);
        previous = Some(byte);
    }
    normalized
}

pub(crate) fn status_badge(status: LastRunStatus) -> &'static str {
    match status {
        LastRunStatus::NeverRun => "○",
        LastRunStatus::Completed => "✓",
        LastRunStatus::MaxIterations => "◉",
        LastRunStatus::Failed => "!",
        LastRunStatus::Canceled => "×",
    }
}

pub(crate) fn status_label(status: LastRunStatus) -> &'static str {
    match status {
        LastRunStatus::NeverRun => "never run",
        LastRunStatus::Completed => "completed",
        LastRunStatus::MaxIterations => "max iterations",
        LastRunStatus::Failed => "failed",
        LastRunStatus::Canceled => "canceled",
    }
}

pub(crate) fn status_style(
    status: LastRunStatus,
    accent: Color,
    success: Color,
    warning: Color,
    muted: Color,
) -> Style {
    match status {
        LastRunStatus::NeverRun => Style::default().fg(muted),
        LastRunStatus::Completed => Style::default().fg(Color::Black).bg(success),
        LastRunStatus::MaxIterations => Style::default().fg(Color::Black).bg(warning),
        LastRunStatus::Failed => Style::default().fg(Color::White).bg(Color::Red),
        LastRunStatus::Canceled => Style::default().fg(accent),
    }
}

pub(crate) fn resolved_accent_color(name: &str) -> Color {
    if name.trim().eq_ignore_ascii_case("cyan") {
        Color::Cyan
    } else {
        color_from_name(name).unwrap_or(Color::Cyan)
    }
}

pub(crate) fn resolved_success_color(name: &str) -> Color {
    if name.trim().eq_ignore_ascii_case("green") {
        Color::LightGreen
    } else {
        color_from_name(name).unwrap_or(Color::LightGreen)
    }
}

pub(crate) fn resolved_warning_color(name: &str) -> Color {
    if name.trim().eq_ignore_ascii_case("yellow") {
        Color::LightYellow
    } else {
        color_from_name(name).unwrap_or(Color::LightYellow)
    }
}

fn color_from_name(name: &str) -> Option<Color> {
    let normalized = name.trim().to_ascii_lowercase();
    Some(match normalized.as_str() {
        "black" => Color::Black,
        "red" => Color::Red,
        "green" => Color::Green,
        "yellow" => Color::Yellow,
        "blue" => Color::Blue,
        "magenta" => Color::Magenta,
        "cyan" => Color::Cyan,
        "gray" | "grey" => Color::Gray,
        "darkgray" | "dark_gray" | "darkgrey" | "dark_grey" => Color::DarkGray,
        "lightred" | "light_red" => Color::LightRed,
        "lightgreen" | "light_green" => Color::LightGreen,
        "lightyellow" | "light_yellow" => Color::LightYellow,
        "lightblue" | "light_blue" => Color::LightBlue,
        "lightmagenta" | "light_magenta" => Color::LightMagenta,
        "lightcyan" | "light_cyan" => Color::LightCyan,
        "white" => Color::White,
        _ => return None,
    })
}
