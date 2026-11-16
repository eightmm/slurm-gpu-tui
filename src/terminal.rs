use std::io::IsTerminal;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

#[derive(Clone, Copy, Default, clap::ValueEnum)]
pub enum ColorMode {
    #[default]
    Auto,
    Always,
    Never,
}
impl ColorMode {
    fn enabled(self, terminal: bool, no_color: bool, dumb: bool) -> bool {
        match self {
            Self::Auto => terminal && !no_color && !dumb,
            Self::Always => true,
            Self::Never => false,
        }
    }
    pub(crate) fn stdout(self) -> bool {
        self.enabled(
            std::io::stdout().is_terminal(),
            std::env::var_os("NO_COLOR").is_some(),
            std::env::var("TERM").is_ok_and(|v| v == "dumb"),
        )
    }
}

#[derive(Clone, Copy)]
pub(crate) enum Tone {
    Normal,
    Muted,
    Heading,
    Mine,
    Good,
    Busy,
    Alert,
    Bold,
    Value,
}

pub(crate) fn paint(text: &str, tone: Tone, color: bool) -> String {
    let text = crate::cli::terminal_safe(text);
    if !color {
        return text;
    }
    let style = match tone {
        Tone::Normal => return text,
        Tone::Muted => "38;5;245",
        Tone::Heading => "38;5;117;1",
        Tone::Mine => "38;5;115;1",
        Tone::Good => "38;5;115",
        Tone::Busy => "38;5;222",
        Tone::Alert => "38;5;210;1",
        Tone::Bold => "1",
        Tone::Value => "38;5;111",
    };
    format!("\x1b[{style}m{text}\x1b[0m")
}

pub(crate) fn reason_tone(reason: &str) -> Tone {
    if ["DependencyNeverSatisfied", "Invalid", "Fail", "Down"]
        .iter()
        .any(|flag| reason.contains(flag))
    {
        Tone::Alert
    } else {
        Tone::Busy
    }
}

pub(crate) fn cell(raw: &str, width: usize, right: bool) -> String {
    let safe: String = raw.chars().filter(|c| !c.is_control()).collect();
    let mut text = String::new();
    let clipped = safe.width() > width;
    let mut remaining = width.saturating_sub(usize::from(clipped));
    for c in safe.chars() {
        let size = c.width().unwrap_or(0);
        if size > remaining {
            break;
        }
        text.push(c);
        remaining -= size;
    }
    if clipped {
        text.push('…');
    }
    let padding = " ".repeat(width.saturating_sub(text.width()));
    if right {
        format!("{padding}{text}")
    } else {
        format!("{text}{padding}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn terminal_colors_respect_pipes_opt_out_and_explicit_overrides() {
        assert!(ColorMode::Auto.enabled(true, false, false));
        assert!(!ColorMode::Auto.enabled(false, false, false));
        assert!(!ColorMode::Auto.enabled(true, true, false));
        assert!(!ColorMode::Auto.enabled(true, false, true));
        assert!(ColorMode::Always.enabled(false, true, true));
        assert!(!ColorMode::Never.enabled(true, false, false));
    }
}
