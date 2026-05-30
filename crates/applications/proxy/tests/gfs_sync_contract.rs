//! Contract test: every `gfs_sync.*` symbol the proxy references must be defined
//! by the clone bootstrap SQL that lives in `gfs-compute-docker`.
//!
//! The proxy's only coupling to GFS is this in-DB API (functions + catalog
//! tables). The two used to live in separate repos, so a rename or signature
//! change in `clone_bootstrap.sql` could silently break the proxy at runtime.
//! Now that both are in the same workspace, this test fails the build instead.
//!
//! It is intentionally self-discovering: it scans the proxy's own sources for
//! `gfs_sync.<name>` references rather than hard-coding a list, so a newly added
//! call with no matching definition is caught automatically.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

/// `clone_bootstrap.sql`, relative to this crate's manifest dir.
const BOOTSTRAP_SQL: &str =
    "../../adapters/compute-docker/src/containers/clone_bootstrap.sql";

/// Pull every `gfs_sync.<ident>` reference out of `text`.
fn referenced_symbols(text: &str) -> BTreeSet<String> {
    const PREFIX: &str = "gfs_sync.";
    let mut out = BTreeSet::new();
    let bytes = text.as_bytes();
    let mut from = 0;
    while let Some(rel) = text[from..].find(PREFIX) {
        let start = from + rel + PREFIX.len();
        let name: String = bytes[start..]
            .iter()
            .take_while(|&&b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
            .map(|&b| b as char)
            .collect();
        if !name.is_empty() {
            out.insert(name);
        }
        from = start;
    }
    out
}

/// Names created by an object-defining `CREATE …` statement in the SQL
/// (tables, views, functions, sequences — not indexes/triggers, which only
/// reference existing objects).
fn defined_symbols(sql: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for line in sql.lines() {
        let l = line.trim_start();
        if !l.starts_with("CREATE ") {
            continue;
        }
        let defines_object = ["TABLE", "VIEW", "FUNCTION", "SEQUENCE"]
            .iter()
            .any(|kw| l.contains(kw));
        if defines_object {
            out.extend(referenced_symbols(line));
        }
    }
    out
}

fn read_proxy_sources() -> String {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut buf = String::new();
    let mut stack = vec![src];
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir).expect("read proxy src dir") {
            let path = entry.unwrap().path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().map(|e| e == "rs").unwrap_or(false) {
                buf.push_str(&fs::read_to_string(&path).expect("read proxy source"));
                buf.push('\n');
            }
        }
    }
    buf
}

#[test]
fn proxy_only_calls_gfs_sync_symbols_the_clone_defines() {
    let sql_path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(BOOTSTRAP_SQL);
    let sql = fs::read_to_string(&sql_path).unwrap_or_else(|e| {
        panic!("cannot read clone bootstrap SQL at {}: {e}", sql_path.display())
    });

    let defined = defined_symbols(&sql);
    let referenced = referenced_symbols(&read_proxy_sources());

    // Sanity: we actually found both sides (guards against a broken path or a
    // refactor that moved every call behind a constant).
    assert!(!defined.is_empty(), "no gfs_sync.* definitions found in {BOOTSTRAP_SQL}");
    assert!(!referenced.is_empty(), "no gfs_sync.* references found in the proxy sources");

    let missing: Vec<&String> = referenced.difference(&defined).collect();
    assert!(
        missing.is_empty(),
        "the proxy references gfs_sync symbols not defined by clone_bootstrap.sql: {missing:?}\n\
         (defined: {defined:?})\n\
         Either the SQL renamed/removed them or the proxy gained a new dependency \
         that the bootstrap must create.",
    );
}
