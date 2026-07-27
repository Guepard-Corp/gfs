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

/// Like `println!` but returns `io::Result<()>` and silently exits on broken pipe.
///
/// When `gfs log` is piped to `head` or `less`, the pipe closes early and the
/// next write returns `BrokenPipe`. We treat that as a clean exit so the user
/// doesn't see a spurious error message.
#[macro_export]
macro_rules! println_safe {
    ($($arg:tt)*) => {{
        use std::io::Write;
        let line = format!($($arg)*);
        let result = writeln!(std::io::stdout(), "{}", line);
        match result {
            Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => {
                std::process::exit(0);
            }
            other => other,
        }
    }};
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

// ---------------------------------------------------------------------------
// Box content formatting (key/value rows)
// ---------------------------------------------------------------------------

/// Format a key/value row for a rounded box panel.
///
/// Contract: `box_row()` expects `content` to already be padded to `box_width`
/// visible chars. These helpers pad raw text first, then apply styling.
pub fn fmt_box_row(label: &str, value: &str, label_width: usize, box_width: usize) -> String {
    let value_w = box_width.saturating_sub(label_width).saturating_sub(1);
    let padded_label = format!("{:<w$}", label, w = label_width);
    let padded_value = format!("{:<w$}", value, w = value_w);
    format!("{} {}", dimmed(&padded_label), padded_value)
}

/// Same as [`fmt_box_row`], but allows a colored value while padding based on `raw_value`.
pub fn fmt_box_row_colored(
    label: &str,
    colored_value: &str,
    raw_value: &str,
    label_width: usize,
    box_width: usize,
) -> String {
    let value_w = box_width.saturating_sub(label_width).saturating_sub(1);
    let padded_label = format!("{:<w$}", label, w = label_width);
    let remaining = value_w.saturating_sub(raw_value.chars().count());
    format!(
        "{} {}{}",
        dimmed(&padded_label),
        colored_value,
        " ".repeat(remaining)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn box_top_without_title_is_a_full_rule() {
        // width+2 horizontal glyphs between the rounded corners.
        assert_eq!(box_top("", 4), format!("  {}{}{}", BOX_TL, BOX_H.repeat(6), BOX_TR));
    }

    #[test]
    fn box_top_with_title_embeds_label_and_closes() {
        let top = box_top("Repository", 20);
        assert!(top.starts_with(&format!("  {BOX_TL}")), "top: {top}");
        assert!(top.contains("─ Repository "), "title embedded: {top}");
        assert!(top.ends_with(BOX_TR), "closes with corner: {top}");
    }

    #[test]
    fn box_bottom_is_a_full_rule() {
        assert_eq!(box_bottom(4), format!("  {}{}{}", BOX_BL, BOX_H.repeat(6), BOX_BR));
    }

    #[test]
    fn box_row_wraps_content_in_verticals() {
        assert_eq!(box_row("hi", 5), format!("  {BOX_V} hi {BOX_V}"));
    }

    #[test]
    fn tbl_rule_joins_columns_with_junctions() {
        // Two columns of width 3 and 5 → 5 and 7 glyphs, separated by the mid junction.
        let rule = tbl_rule(&[3, 5], TBL_TL, TBL_T_DOWN, TBL_TR);
        let expected = format!(
            "  {}{}{}{}{}",
            TBL_TL,
            TBL_H.repeat(5),
            TBL_T_DOWN,
            TBL_H.repeat(7),
            TBL_TR
        );
        assert_eq!(rule, expected);
    }

    #[test]
    fn fmt_box_row_keeps_label_and_value() {
        let row = fmt_box_row("Branch", "main", 8, 20);
        assert!(row.contains("Branch"), "label present: {row}");
        assert!(row.contains("main"), "value present: {row}");
    }

    #[test]
    fn color_mode_never_disables_color() {
        // `Never` short-circuits to false regardless of TTY or NO_COLOR.
        assert!(!ColorMode::Never.use_color());
    }
}
