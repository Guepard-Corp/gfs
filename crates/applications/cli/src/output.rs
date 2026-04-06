//! Centralized color and output styling for the GFS CLI.
//!
//! Respects TTY detection, NO_COLOR env var, and --color flag.
//! Use the style helpers (cyan, dimmed, etc.) so output respects set_override.

use std::io::IsTerminal;

use owo_colors::OwoColorize;
use owo_colors::Stream;

/// Color mode for CLI output (git-style).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, clap::ValueEnum)]
pub enum ColorMode {
    /// Always use colors (unless NO_COLOR is set)
    Always,
    /// Use colors only when stdout is a TTY and NO_COLOR is not set
    #[default]
    Auto,
    /// Never use colors
    Never,
}

impl ColorMode {
    /// Resolve whether colors should be enabled based on mode, TTY, and NO_COLOR.
    pub fn use_color(self) -> bool {
        if std::env::var("NO_COLOR").is_ok() {
            return false;
        }
        match self {
            ColorMode::Always => true,
            ColorMode::Never => false,
            ColorMode::Auto => std::io::stdout().is_terminal(),
        }
    }

    /// Initialize owo-colors global override. Call once at start of run().
    pub fn init(self) {
        owo_colors::set_override(self.use_color());
    }
}

// ---------------------------------------------------------------------------
// Style helpers - use these instead of .cyan() etc. so set_override is respected.
// Return String to satisfy if_supports_color's 'static lifetime requirement.
// ---------------------------------------------------------------------------

/// Conditionally apply cyan (respects --color, NO_COLOR, TTY).
pub fn cyan(s: impl AsRef<str>) -> String {
    let s = s.as_ref().to_string();
    format!("{}", s.if_supports_color(Stream::Stdout, |t| t.cyan()))
}

/// Conditionally apply dimmed (respects --color, NO_COLOR, TTY).
pub fn dimmed(s: impl AsRef<str>) -> String {
    let s = s.as_ref().to_string();
    format!("{}", s.if_supports_color(Stream::Stdout, |t| t.dimmed()))
}

/// Conditionally apply green (respects --color, NO_COLOR, TTY).
pub fn green(s: impl AsRef<str>) -> String {
    let s = s.as_ref().to_string();
    format!("{}", s.if_supports_color(Stream::Stdout, |t| t.green()))
}

/// Conditionally apply red (respects --color, NO_COLOR, TTY).
pub fn red(s: impl AsRef<str>) -> String {
    let s = s.as_ref().to_string();
    format!("{}", s.if_supports_color(Stream::Stdout, |t| t.red()))
}

/// Conditionally apply yellow (respects --color, NO_COLOR, TTY).
pub fn yellow(s: impl AsRef<str>) -> String {
    let s = s.as_ref().to_string();
    format!("{}", s.if_supports_color(Stream::Stdout, |t| t.yellow()))
}

/// Conditionally apply bold (respects --color, NO_COLOR, TTY).
pub fn bold(s: impl AsRef<str>) -> String {
    let s = s.as_ref().to_string();
    format!("{}", s.if_supports_color(Stream::Stdout, |t| t.bold()))
}

/// Brand accent: bright yellow (GFS gold #ffcb51 mapped to ANSI).
pub fn gold(s: impl AsRef<str>) -> String {
    let s = s.as_ref().to_string();
    format!(
        "{}",
        s.if_supports_color(Stream::Stdout, |t| t.bright_yellow())
    )
}

/// Section header: bold + underline.
pub fn header(s: impl AsRef<str>) -> String {
    // Apply bold and underline separately to avoid chained-temporary issues.
    let s = s.as_ref().to_string();
    let bolded = format!("{}", s.if_supports_color(Stream::Stdout, |t| t.bold()));
    format!(
        "{}",
        bolded.if_supports_color(Stream::Stdout, |t| t.underline())
    )
}

// ---------------------------------------------------------------------------
// Unicode box-drawing characters
// ---------------------------------------------------------------------------

// Rounded corners for info panels / status boxes
pub const BOX_TL: &str = "╭";
pub const BOX_TR: &str = "╮";
pub const BOX_BL: &str = "╰";
pub const BOX_BR: &str = "╯";
pub const BOX_H: &str = "─";
pub const BOX_V: &str = "│";

// Sharp corners for data tables
pub const TBL_TL: &str = "┌";
pub const TBL_TR: &str = "┐";
pub const TBL_BL: &str = "└";
pub const TBL_BR: &str = "┘";
pub const TBL_V: &str = "│";
pub const TBL_H: &str = "─";
pub const TBL_CROSS: &str = "┼";
pub const TBL_T_DOWN: &str = "┬";
pub const TBL_T_UP: &str = "┴";
pub const TBL_T_RIGHT: &str = "├";
pub const TBL_T_LEFT: &str = "┤";

// ---------------------------------------------------------------------------
// Box-drawing helpers (rounded panels)
// ---------------------------------------------------------------------------

/// Top edge of a rounded box with an optional inline title.
/// Example: `╭─ Repository ──────────────────╮`
pub fn box_top(title: &str, width: usize) -> String {
    if title.is_empty() {
        format!("  {}{}{}", BOX_TL, BOX_H.repeat(width + 2), BOX_TR)
    } else {
        let label = format!("{} {} ", BOX_H, title);
        let fill = width + 2 - label.chars().count().min(width + 2);
        format!("  {}{}{}{}", BOX_TL, label, BOX_H.repeat(fill), BOX_TR)
    }
}

/// A row inside a rounded box. `content` should be pre-padded to `width` visible chars.
/// Example: `│ Branch               main     │`
pub fn box_row(content: &str, width: usize) -> String {
    // NOTE: content may contain ANSI escapes. Callers must ensure
    // the *visible* portion is exactly `width` characters wide
    // (pad raw text first, then apply color).
    let _ = width; // used by callers for formatting
    format!("  {} {} {}", BOX_V, content, BOX_V)
}

/// Bottom edge of a rounded box.
/// Example: `╰──────────────────────────────╯`
pub fn box_bottom(width: usize) -> String {
    format!("  {}{}{}", BOX_BL, BOX_H.repeat(width + 2), BOX_BR)
}

// ---------------------------------------------------------------------------
// Table-drawing helpers (sharp-cornered data tables)
// ---------------------------------------------------------------------------

/// Build a horizontal table rule from column widths.
/// `left`, `mid`, `right` are the corner/junction characters.
pub fn tbl_rule(cols: &[usize], left: &str, mid: &str, right: &str) -> String {
    let segments: Vec<String> = cols.iter().map(|&w| TBL_H.repeat(w + 2)).collect();
    format!("  {}{}{}", left, segments.join(mid), right)
}
