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
use ratatui::{
    Terminal,
    backend::CrosstermBackend,
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
