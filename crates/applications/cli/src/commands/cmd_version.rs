//! `gfs version` — print the CLI version with retro arcade-style ASCII art.

use crate::output::{bold, dimmed, gold};
use crate::println_safe;
use anyhow::Result;

/// Print the current gfs CLI version inside a retro branded box.
pub fn run() -> Result<()> {
    let version = env!("CARGO_PKG_VERSION");
    let target = format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH);

    // Double-line box (DOS/retro style)
    let tl = "╔";
    let tr = "╗";
    let bl = "╚";
    let br = "╝";
    let h = "═";
    let v = "║";

    let indent = "    ";
    let tagline = "Git For database Systems";
    let meta = format!("v{} · rust · {}", version, target);

    let art = [
        " ██████  ███████ ███████",
        "██       ██      ██     ",
        "██  ███  █████   ███████",
        "██   ██  ██           ██",
        " ██████  ██      ███████",
    ];

    let min_w: usize = 39;
    let art_w = art
        .iter()
        .map(|line| indent.len() + line.chars().count())
        .max()
        .unwrap_or(0);
    let w: usize = min_w
        .max(indent.len() + tagline.chars().count())
        .max(indent.len() + meta.chars().count())
        .max(art_w);

    // Top border
    println_safe!("  {}{}{}", tl, h.repeat(w + 2), tr)?;

    // Empty line
    println_safe!("  {} {} {}", v, " ".repeat(w), v)?;

    // ASCII art lines (gold-colored block letters)
    for line in &art {
        let plain_len = indent.len() + line.chars().count();
        let remaining = w.saturating_sub(plain_len);
        println_safe!(
            "  {} {}{}{} {}",
            v,
            indent,
            gold(line),
            " ".repeat(remaining),
            v
        )?;
    }

    // Empty line
    println_safe!("  {} {} {}", v, " ".repeat(w), v)?;

    // Tagline (bold)
    {
        let plain_len = indent.len() + tagline.chars().count();
        let remaining = w.saturating_sub(plain_len);
        println_safe!(
            "  {} {}{}{} {}",
            v,
            indent,
            bold(tagline),
            " ".repeat(remaining),
            v
        )?;
    }

    // Version + platform (dimmed)
    {
        let plain_len = indent.len() + meta.chars().count();
        let remaining = w.saturating_sub(plain_len);
        println_safe!(
            "  {} {}{}{} {}",
            v,
            indent,
            dimmed(&meta),
            " ".repeat(remaining),
            v
        )?;
    }

    // Empty line
    println_safe!("  {} {} {}", v, " ".repeat(w), v)?;

    // Bottom border
    println_safe!("  {}{}{}", bl, h.repeat(w + 2), br)?;
    Ok(())
}
