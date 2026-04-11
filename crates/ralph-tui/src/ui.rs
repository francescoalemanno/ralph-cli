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
use ralph_core::ThemeColor;
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

pub(crate) fn ratatui_color(color: ThemeColor) -> Color {
    match color {
        ThemeColor::Black => Color::Black,
        ThemeColor::Red => Color::Red,
        ThemeColor::Green => Color::Green,
        ThemeColor::Yellow => Color::Yellow,
        ThemeColor::Blue => Color::Blue,
        ThemeColor::Magenta => Color::Magenta,
        ThemeColor::Cyan => Color::Cyan,
        ThemeColor::Gray => Color::Gray,
        ThemeColor::DarkGray => Color::DarkGray,
        ThemeColor::LightRed => Color::LightRed,
        ThemeColor::LightGreen => Color::LightGreen,
        ThemeColor::LightYellow => Color::LightYellow,
        ThemeColor::LightBlue => Color::LightBlue,
        ThemeColor::LightMagenta => Color::LightMagenta,
        ThemeColor::LightCyan => Color::LightCyan,
        ThemeColor::White => Color::White,
    }
}
