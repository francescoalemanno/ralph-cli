use std::env;

use serde::{Deserialize, Serialize};

use crate::config::ThemeConfig;

const RALPH_THEME_MODE_ENV: &str = "RALPH_THEME_MODE";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ThemeMode {
    #[default]
    Auto,
    Dark,
    Light,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThemeVariant {
    Dark,
    Light,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThemeColor {
    Black,
    Red,
    Green,
    Yellow,
    Blue,
    Magenta,
    Cyan,
    Gray,
    DarkGray,
    LightRed,
    LightGreen,
    LightYellow,
    LightBlue,
    LightMagenta,
    LightCyan,
    White,
}

impl ThemeColor {
    pub fn parse(name: &str) -> Option<Self> {
        let normalized = name.trim().to_ascii_lowercase();
        Some(match normalized.as_str() {
            "black" => Self::Black,
            "red" => Self::Red,
            "green" => Self::Green,
            "yellow" => Self::Yellow,
            "blue" => Self::Blue,
            "magenta" => Self::Magenta,
            "cyan" => Self::Cyan,
            "gray" | "grey" => Self::Gray,
            "darkgray" | "dark_gray" | "darkgrey" | "dark_grey" => Self::DarkGray,
            "lightred" | "light_red" => Self::LightRed,
            "lightgreen" | "light_green" => Self::LightGreen,
            "lightyellow" | "light_yellow" => Self::LightYellow,
            "lightblue" | "light_blue" => Self::LightBlue,
            "lightmagenta" | "light_magenta" => Self::LightMagenta,
            "lightcyan" | "light_cyan" => Self::LightCyan,
            "white" => Self::White,
            _ => return None,
        })
    }

    pub fn contrast(self) -> Self {
        if self.is_light() {
            Self::Black
        } else {
            Self::White
        }
    }

    pub fn ansi_fg_code(self) -> u8 {
        match self {
            Self::Black => 30,
            Self::Red => 31,
            Self::Green => 32,
            Self::Yellow => 33,
            Self::Blue => 34,
            Self::Magenta => 35,
            Self::Cyan => 36,
            Self::Gray => 37,
            Self::DarkGray => 90,
            Self::LightRed => 91,
            Self::LightGreen => 92,
            Self::LightYellow => 93,
            Self::LightBlue => 94,
            Self::LightMagenta => 95,
            Self::LightCyan => 96,
            Self::White => 97,
        }
    }

    fn is_light(self) -> bool {
        matches!(
            self,
            Self::Yellow
                | Self::Cyan
                | Self::Gray
                | Self::LightRed
                | Self::LightGreen
                | Self::LightYellow
                | Self::LightBlue
                | Self::LightMagenta
                | Self::LightCyan
                | Self::White
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResolvedTheme {
    pub variant: ThemeVariant,
    pub background: ThemeColor,
    pub text: ThemeColor,
    pub muted: ThemeColor,
    pub subtle: ThemeColor,
    pub accent: ThemeColor,
    pub success: ThemeColor,
    pub warning: ThemeColor,
    pub error: ThemeColor,
}

impl ThemeConfig {
    pub fn resolve(&self) -> ResolvedTheme {
        ResolvedTheme::from_config(self)
    }
}

impl ThemeMode {
    pub fn resolve(self) -> ThemeVariant {
        self.resolve_with(
            env::var(RALPH_THEME_MODE_ENV).ok().as_deref(),
            env::var("COLORFGBG").ok().as_deref(),
        )
    }

    fn resolve_with(self, override_mode: Option<&str>, colorfgbg: Option<&str>) -> ThemeVariant {
        match self {
            Self::Dark => ThemeVariant::Dark,
            Self::Light => ThemeVariant::Light,
            Self::Auto => {
                detect_theme_variant(override_mode, colorfgbg).unwrap_or(ThemeVariant::Dark)
            }
        }
    }
}

impl ResolvedTheme {
    pub fn from_config(config: &ThemeConfig) -> Self {
        let variant = config.mode.resolve();
        let (background, text, muted, subtle) = match variant {
            ThemeVariant::Dark => (
                ThemeColor::Black,
                ThemeColor::White,
                ThemeColor::Gray,
                ThemeColor::DarkGray,
            ),
            ThemeVariant::Light => (
                ThemeColor::White,
                ThemeColor::Black,
                ThemeColor::DarkGray,
                ThemeColor::Gray,
            ),
        };

        Self {
            variant,
            background,
            text,
            muted,
            subtle,
            accent: resolve_semantic_color(
                &config.accent_color,
                variant,
                ThemeColor::Cyan,
                ThemeColor::Blue,
                &["cyan"],
            ),
            success: resolve_semantic_color(
                &config.success_color,
                variant,
                ThemeColor::LightGreen,
                ThemeColor::Green,
                &["green"],
            ),
            warning: resolve_semantic_color(
                &config.warning_color,
                variant,
                ThemeColor::LightYellow,
                ThemeColor::Magenta,
                &["yellow"],
            ),
            error: resolve_semantic_color(
                &config.error_color,
                variant,
                ThemeColor::LightRed,
                ThemeColor::Red,
                &["red"],
            ),
        }
    }
}

fn detect_theme_variant(
    override_mode: Option<&str>,
    colorfgbg: Option<&str>,
) -> Option<ThemeVariant> {
    if let Some(mode) = override_mode.and_then(parse_theme_mode_override) {
        return Some(mode);
    }
    colorfgbg.and_then(theme_variant_from_colorfgbg)
}

fn parse_theme_mode_override(value: &str) -> Option<ThemeVariant> {
    match value.trim().to_ascii_lowercase().as_str() {
        "dark" => Some(ThemeVariant::Dark),
        "light" => Some(ThemeVariant::Light),
        _ => None,
    }
}

fn theme_variant_from_colorfgbg(value: &str) -> Option<ThemeVariant> {
    let background = value
        .split(';')
        .rev()
        .find_map(|part| part.trim().parse::<u8>().ok())?;
    Some(classify_xterm_background(background))
}

fn classify_xterm_background(index: u8) -> ThemeVariant {
    let (red, green, blue) = xterm_rgb(index);
    let brightness = (u32::from(red) * 299 + u32::from(green) * 587 + u32::from(blue) * 114) / 1000;
    if brightness >= 128 {
        ThemeVariant::Light
    } else {
        ThemeVariant::Dark
    }
}

fn xterm_rgb(index: u8) -> (u8, u8, u8) {
    const ANSI_16: [(u8, u8, u8); 16] = [
        (0x00, 0x00, 0x00),
        (0xcd, 0x00, 0x00),
        (0x00, 0xcd, 0x00),
        (0xcd, 0xcd, 0x00),
        (0x00, 0x00, 0xee),
        (0xcd, 0x00, 0xcd),
        (0x00, 0xcd, 0xcd),
        (0xe5, 0xe5, 0xe5),
        (0x7f, 0x7f, 0x7f),
        (0xff, 0x00, 0x00),
        (0x00, 0xff, 0x00),
        (0xff, 0xff, 0x00),
        (0x5c, 0x5c, 0xff),
        (0xff, 0x00, 0xff),
        (0x00, 0xff, 0xff),
        (0xff, 0xff, 0xff),
    ];
    const COLOR_STEPS: [u8; 6] = [0, 95, 135, 175, 215, 255];

    match index {
        0..=15 => ANSI_16[index as usize],
        16..=231 => {
            let index = index - 16;
            let red = COLOR_STEPS[(index / 36) as usize];
            let green = COLOR_STEPS[((index % 36) / 6) as usize];
            let blue = COLOR_STEPS[(index % 6) as usize];
            (red, green, blue)
        }
        232..=255 => {
            let gray = 8 + (index - 232) * 10;
            (gray, gray, gray)
        }
    }
}

fn resolve_semantic_color(
    configured: &str,
    variant: ThemeVariant,
    dark_default: ThemeColor,
    light_default: ThemeColor,
    semantic_names: &[&str],
) -> ThemeColor {
    let default = match variant {
        ThemeVariant::Dark => dark_default,
        ThemeVariant::Light => light_default,
    };

    let normalized = configured.trim();
    if normalized.is_empty() {
        return default;
    }

    let normalized = normalized.to_ascii_lowercase();
    if semantic_names.iter().any(|name| normalized == *name) {
        return default;
    }

    if let Some(color) = ThemeColor::parse(&normalized) {
        return color;
    }

    default
}

#[cfg(test)]
mod tests {
    use super::{
        ResolvedTheme, ThemeColor, ThemeMode, ThemeVariant, classify_xterm_background,
        theme_variant_from_colorfgbg,
    };
    use crate::ThemeConfig;

    #[test]
    fn auto_theme_defaults_to_dark_without_signals() {
        assert_eq!(ThemeMode::Auto.resolve_with(None, None), ThemeVariant::Dark);
    }

    #[test]
    fn explicit_theme_mode_wins() {
        assert_eq!(
            ThemeMode::Dark.resolve_with(Some("light"), Some("15;0")),
            ThemeVariant::Dark
        );
        assert_eq!(
            ThemeMode::Light.resolve_with(Some("dark"), Some("0;15")),
            ThemeVariant::Light
        );
    }

    #[test]
    fn env_override_beats_detected_background_for_auto_mode() {
        assert_eq!(
            ThemeMode::Auto.resolve_with(Some("light"), Some("15;0")),
            ThemeVariant::Light
        );
    }

    #[test]
    fn colorfgbg_detects_light_and_dark_backgrounds() {
        assert_eq!(
            theme_variant_from_colorfgbg("15;0"),
            Some(ThemeVariant::Dark)
        );
        assert_eq!(
            theme_variant_from_colorfgbg("0;15"),
            Some(ThemeVariant::Light)
        );
    }

    #[test]
    fn xterm_palette_classification_handles_extended_indexes() {
        assert_eq!(classify_xterm_background(232), ThemeVariant::Dark);
        assert_eq!(classify_xterm_background(255), ThemeVariant::Light);
    }

    #[test]
    fn resolved_theme_adapts_legacy_defaults_between_variants() {
        let config = ThemeConfig::default();

        let dark = ResolvedTheme::from_config(&ThemeConfig {
            mode: ThemeMode::Dark,
            ..config.clone()
        });
        let light = ResolvedTheme::from_config(&ThemeConfig {
            mode: ThemeMode::Light,
            ..config
        });

        assert_eq!(dark.background, ThemeColor::Black);
        assert_eq!(dark.text, ThemeColor::White);
        assert_eq!(dark.accent, ThemeColor::Cyan);
        assert_eq!(dark.success, ThemeColor::LightGreen);
        assert_eq!(dark.warning, ThemeColor::LightYellow);
        assert_eq!(dark.error, ThemeColor::LightRed);

        assert_eq!(light.background, ThemeColor::White);
        assert_eq!(light.text, ThemeColor::Black);
        assert_eq!(light.accent, ThemeColor::Blue);
        assert_eq!(light.success, ThemeColor::Green);
        assert_eq!(light.warning, ThemeColor::Magenta);
        assert_eq!(light.error, ThemeColor::Red);
    }

    #[test]
    fn configured_non_default_colors_are_preserved() {
        let resolved = ResolvedTheme::from_config(&ThemeConfig {
            mode: ThemeMode::Light,
            accent_color: "light_cyan".to_owned(),
            success_color: "blue".to_owned(),
            warning_color: "red".to_owned(),
            error_color: "dark_gray".to_owned(),
        });

        assert_eq!(resolved.accent, ThemeColor::LightCyan);
        assert_eq!(resolved.success, ThemeColor::Blue);
        assert_eq!(resolved.warning, ThemeColor::Red);
        assert_eq!(resolved.error, ThemeColor::DarkGray);
    }

    #[test]
    fn theme_color_contrast_picks_legible_text() {
        assert_eq!(ThemeColor::Black.contrast(), ThemeColor::White);
        assert_eq!(ThemeColor::LightYellow.contrast(), ThemeColor::Black);
    }
}
