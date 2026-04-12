use std::{
    env,
    io::{self, IsTerminal},
};

use crate::{LastRunStatus, ResolvedTheme, ThemeColor, ThemeConfig};

#[derive(Debug, Clone, Copy)]
pub struct TerminalTheme {
    colors_enabled: bool,
    palette: ResolvedTheme,
}

impl TerminalTheme {
    pub fn new(theme_config: &ThemeConfig) -> Self {
        Self {
            colors_enabled: io::stdout().is_terminal() && env::var_os("NO_COLOR").is_none(),
            palette: theme_config.resolve(),
        }
    }

    pub fn palette(self) -> ResolvedTheme {
        self.palette
    }

    pub fn style(self) -> AnsiStyle {
        AnsiStyle {
            enabled: self.colors_enabled,
            ..AnsiStyle::default()
        }
    }

    pub fn label_style(self) -> AnsiStyle {
        self.style().fg(self.palette.subtle)
    }

    pub fn status_color(self, status: LastRunStatus) -> ThemeColor {
        match status {
            LastRunStatus::NeverRun | LastRunStatus::Canceled => self.palette.accent,
            LastRunStatus::Completed => self.palette.success,
            LastRunStatus::MaxIterations => self.palette.warning,
            LastRunStatus::Failed => self.palette.error,
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct AnsiStyle {
    enabled: bool,
    fg: Option<ThemeColor>,
    bold: bool,
}

impl AnsiStyle {
    pub fn fg(mut self, color: ThemeColor) -> Self {
        self.fg = Some(color);
        self
    }

    pub fn bold(mut self) -> Self {
        self.bold = true;
        self
    }

    pub fn paint(self, text: impl Into<String>) -> String {
        let text = text.into();
        if !self.enabled {
            return text;
        }

        let mut codes = Vec::new();
        if self.bold {
            codes.push(1u16);
        }
        if let Some(color) = self.fg {
            codes.push(u16::from(color.ansi_fg_code()));
        }
        if codes.is_empty() {
            return text;
        }

        let codes = codes
            .into_iter()
            .map(|code| code.to_string())
            .collect::<Vec<_>>()
            .join(";");
        format!("\u{1b}[{codes}m{text}\u{1b}[0m")
    }
}
