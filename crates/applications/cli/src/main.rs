//! `gfs` – GFS command-line interface binary.
//!
//! Thin wrapper around the library. See `gfs_cli::run()` for programmatic use.

use std::ffi::OsString;

use gfs_cli::output::red;
use serde_json::json;

fn wants_json(args: &[String]) -> bool {
    for a in args {
        if a == "--" {
            break;
        }
        if a == "--json" {
            return true;
        }
        if let Some(rest) = a.strip_prefix("--json=") {
            let v = rest.trim().to_ascii_lowercase();
            return matches!(v.as_str(), "1" | "true" | "yes" | "on");
        }
    }
    false
}

/// The arguments as UTF-8, or the position of the first one that is not.
///
/// `std::env::args()` panics on such an argument, which exited 101 -- the code
/// of any crash -- with a message that echoed the undecodable bytes back. The
/// position is counted from 1 for the first argument after `gfs`.
///
/// argv[0] is exempt: it is the path the binary was launched by, not something
/// the user typed, and clap only uses it for display.
fn utf8_args(raw: Vec<OsString>) -> Result<Vec<String>, usize> {
    raw.into_iter()
        .enumerate()
        .map(|(position, arg)| match arg.into_string() {
            Ok(arg) => Ok(arg),
            Err(arg) if position == 0 => Ok(arg.to_string_lossy().into_owned()),
            Err(_) => Err(position),
        })
        .collect()
}

/// A usage error, so it takes the same exit code and rendering as a value clap
/// could not parse. It names the position only: the bytes are not echoed.
fn non_utf8_argument(position: usize) -> clap::Error {
    clap::Error::raw(
        clap::error::ErrorKind::InvalidUtf8,
        format!("argument {position} is not valid UTF-8\n"),
    )
}

#[tokio::main]
async fn main() {
    // rustls 0.23 requires an explicit process-level CryptoProvider when both
    // aws-lc-rs and ring are present in the dependency graph (kube pulls one,
    // other deps pull the other). kube's TLS client defaults to aws-lc-rs, so
    // install it before any TLS connection is attempted, or rustls panics.
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();

    // Tracing goes to stderr by default. CLI consumers that scrape stderr for
    // error messages can override the level via RUST_LOG to silence INFO logs,
    // or via GFS_LOG to a stricter default. WARN+ERROR always pass through so
    // genuine failures are visible. ANSI is suppressed when stderr is not a tty
    let default_filter = std::env::var("GFS_LOG").unwrap_or_else(|_| "warn".to_string());
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(default_filter)),
        )
        .with_writer(std::io::stderr)
        .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stderr()))
        .init();

    let raw: Vec<OsString> = std::env::args_os().collect();
    // Lossy is safe here: a replacement character can never produce "--json".
    let lossy: Vec<String> = raw
        .iter()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect();
    let wants_json = wants_json(&lossy);

    let result = match utf8_args(raw) {
        Ok(args) => gfs_cli::run(args).await,
        Err(position) => Err(non_utf8_argument(position).into()),
    };

    match result {
        Ok(exit_code) => std::process::exit(exit_code),
        Err(err) => {
            if wants_json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&json!({
                        "error": {
                            "message": err.to_string(),
                            "details": format!("{err:#}"),
                        }
                    }))
                    .unwrap_or_else(|_| "{\"error\":{\"message\":\"serialization failed\"}}".into())
                );
            } else if let Some(parse) = err.downcast_ref::<clap::Error>().filter(|p| {
                p.kind() != clap::error::ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
            }) {
                // A parse failure already carries clap's own "error:", so
                // prefixing it again printed "error: error: ...".
                //
                // `lib.rs` returns early for DisplayHelp and DisplayVersion, so
                // every other kind arrives here -- including
                // DisplayHelpOnMissingArgumentOrSubcommand (`gfs storage` with no
                // subcommand), which renders bare help and carries no prefix at
                // all. Deferring unconditionally would drop the "error:" line
                // those commands owe.
                //
                // Match on the kind, not on the rendered text. `ErrorKind` is
                // `#[non_exhaustive]` and grows additively, so a new kind costs
                // one doubled prefix; clap's formatting carries no such guarantee,
                // and a change to it would silently restore the doubling on every
                // command at once.
                let _ = parse.print();
            } else {
                eprintln!("{} {err}", red("error:"));
            }
            // A usage error exits 3, not 1.
            //
            // 1 is a statement that the command RAN and found something: fsck
            // documents it as "unreachable objects found -- a collector would have
            // work to do". A mistyped flag used to land on that same code, so
            // `gfs fsck --typo` was indistinguishable from a repository with
            // collectable garbage, and a script branching on 1 to run a collector
            // would be triggered by a typo.
            //
            // 3 already means "the command could not be completed, so this says
            // nothing about the repository", which is exactly what a rejected
            // argument list is. `--help` and `--version` never reach here: `run()`
            // returns Ok(0) for DisplayHelp and DisplayVersion before this point.
            //
            // Only parse failures move. Every other error keeps 1, so this does not
            // silently redefine the code for the errors that did run.
            let usage_error = err.downcast_ref::<clap::Error>().is_some();
            std::process::exit(if usage_error { 3 } else { 1 });
        }
    }
}
