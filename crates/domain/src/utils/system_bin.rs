//! Resolving system utilities to a known path instead of trusting `PATH`.
//!
//! GFS shells out to a handful of coreutils — `cp` above all — and which binary
//! answers to that name decides whether a commit works. `cp -cRp` asks for a
//! clonefile(2) copy and is a BSD flag: GNU `cp` rejects it outright with
//! `cp: invalid option -- 'c'`. Homebrew's coreutils installs a GNU `cp` at
//! `/opt/homebrew/opt/coreutils/libexec/gnubin`, which many developers put
//! ahead of `/usr/bin`, and every `gfs commit` on such a machine failed with an
//! error that named a `cp` invocation and nothing about `PATH` — so there was
//! no way to connect the two.
//!
//! The general form of the bug is worse than that one flag: resolving a helper
//! through `PATH` makes the behaviour of a commit depend on the user's shell
//! configuration, which is not a variable a storage backend should have.

use std::path::{Path, PathBuf};

/// Where a system utility is expected to live, most-standard first.
const STANDARD_DIRS: [&str; 2] = ["/bin", "/usr/bin"];

/// Resolve `name` to an absolute path, or fall back to `name` itself.
///
/// Prefers `/bin/<name>` then `/usr/bin/<name>`, which is where the coreutils
/// these call sites are written against live on macOS and on mainstream Linux.
///
/// Falling back rather than failing is deliberate. On macOS `/bin/cp` is on the
/// read-only system volume and always there, so the fallback never fires. On
/// Linux it is *not* guaranteed — NixOS ships no `/bin/cp` at all, providing
/// only `/bin/sh` — so hardcoding an absolute path would trade a bug that
/// affects developers with coreutils on `PATH` for one that affects every NixOS
/// user. Preferring the standard location and degrading to `PATH` fixes the
/// first without creating the second.
pub fn resolve(name: &str) -> PathBuf {
    for dir in STANDARD_DIRS {
        let candidate = Path::new(dir).join(name);
        if candidate.is_file() {
            return candidate;
        }
    }
    PathBuf::from(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// On any platform these tests run on, `cp` is in a standard location, so
    /// resolution must not fall through to a bare name.
    #[test]
    fn a_standard_utility_resolves_to_an_absolute_path() {
        let cp = resolve("cp");
        assert!(
            cp.is_absolute(),
            "cp should resolve to a standard location, got {cp:?}"
        );
        assert!(cp.is_file(), "{cp:?} should exist");
    }

    /// The fallback is what keeps this safe on a system with no `/bin`, so it
    /// has to actually fall back rather than return something that cannot run.
    #[test]
    fn an_absent_utility_falls_back_to_the_bare_name() {
        let name = "gfs-definitely-not-a-real-binary";
        assert_eq!(resolve(name), PathBuf::from(name));
    }

    /// Resolution must not be satisfied by a directory that happens to share
    /// the name.
    #[test]
    fn a_directory_is_not_mistaken_for_a_utility() {
        // `/bin` and `/usr/bin` both exist as directories; asking for the empty
        // name would join to the directory itself.
        assert_eq!(resolve(""), PathBuf::from(""));
    }
}
