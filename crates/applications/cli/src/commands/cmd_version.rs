//! `gfs version` — print the CLI version with retro arcade-style ASCII art.

use crate::output::{bold, dimmed, gold};

/// Print the current gfs CLI version inside a retro branded box.
pub fn run() {
    let version = env!("CARGO_PKG_VERSION");
    let target = format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH);

    // Double-line box (DOS/retro style)
    let tl = "╔";
    let tr = "╗";
    let bl = "╚";
    let br = "╝";
    let h = "═";
    let v = "║";

    let w: usize = 39; // inner visible width

    let art = [
        " ██████  ███████ ███████",
        "██       ██      ██     ",
        "██  ███  █████   ███████",
        "██   ██  ██           ██",
        " ██████  ██      ███████",
    ];

    let tagline = "Git For database Systems";
    let meta = format!("v{} · rust · {}", version, target);

    // Top border
    println!("  {}{}{}", tl, h.repeat(w + 2), tr);

    // Empty line
    println!("  {} {} {}", v, " ".repeat(w), v);

    // ASCII art lines (gold-colored block letters)
    for line in &art {
        let indent = "    ";
        let plain_len = indent.len() + line.chars().count();
        let remaining = w.saturating_sub(plain_len);
        println!(
            "  {} {}{}{} {}",
            v,
            indent,
            gold(line),
            " ".repeat(remaining),
            v
        );
    }

    // Empty line
    println!("  {} {} {}", v, " ".repeat(w), v);

    // Tagline (bold)
    {
        let indent = "    ";
        let plain_len = indent.len() + tagline.chars().count();
        let remaining = w.saturating_sub(plain_len);
        println!(
            "  {} {}{}{} {}",
            v,
            indent,
            bold(tagline),
            " ".repeat(remaining),
            v
        );
    }

    // Version + platform (dimmed)
    {
        let indent = "    ";
        let plain_len = indent.len() + meta.chars().count();
        let remaining = w.saturating_sub(plain_len);
        println!(
            "  {} {}{}{} {}",
            v,
            indent,
            dimmed(&meta),
            " ".repeat(remaining),
            v
        );
    }

    // Empty line
    println!("  {} {} {}", v, " ".repeat(w), v);

    // Bottom border
    println!("  {}{}{}", bl, h.repeat(w + 2), br);
}
