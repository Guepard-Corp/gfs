//! Checkout's uncommitted-work check against a real PostgreSQL: what a read
//! leaves behind is not work, and what a write leaves behind is.
//!
//! Its own binary rather than part of `e2e_checkout_postgres`: libtest's "has
//! been running for over 60 seconds" clock starts while a `#[serial]` test waits
//! for the lock, so sharing a binary pushed the lifecycle test past it sooner.
//!
//! macOS-only: commit uses the APFS storage backend. Docker or Podman must be running.

#![cfg(target_os = "macos")]

mod common;

use common::cli_runner;

/// Reading rows is not uncommitted work; writing them still is.
///
/// PostgreSQL marks a row's inserting transaction committed the first time it
/// reads the row and writes that page back at the next checkpoint or shutdown,
/// so a `SELECT` after a commit changed the table's file. Checkout read that as
/// work it would overwrite and refused the next return to the branch.
#[test]
fn a_read_after_a_commit_is_not_uncommitted_work() {
    common::postgres::with_fresh_repo(|repo_path| {
        // The container is replaced on checkout, so look it up for each call.
        let psql = |sql: &str| {
            common::postgres::run_psql_select(&common::postgres::get_container_id(repo_path), sql)
        };
        psql(
            "CREATE TABLE t (id int, v text); \
             INSERT INTO t SELECT g, 'row ' || g FROM generate_series(1, 1000) g",
        );
        let (ok, _, stderr) = cli_runner::gfs_commit(repo_path, "rows", None, None);
        assert!(ok, "commit should succeed; stderr: {stderr}");

        let count = psql("SELECT count(*) FROM t");
        assert!(
            count.contains("1000"),
            "the rows should read back; got: {count}"
        );
        let (ok, _, stderr) = cli_runner::gfs_checkout_b(repo_path, "reader", None);
        assert!(ok, "checkout -b reader should succeed; stderr: {stderr}");
        let (ok, _, stderr) = cli_runner::gfs_checkout(repo_path, "main");
        assert!(ok, "a read is not uncommitted work; stderr: {stderr}");

        // Same length, so only the page contents can tell it changed.
        psql("UPDATE t SET v = 'ROW ' || id WHERE id = 1");
        let (ok, _, stderr) = cli_runner::gfs_checkout(repo_path, "reader");
        assert!(ok, "leaving main should succeed; stderr: {stderr}");
        let (ok, _, stderr) = cli_runner::gfs_checkout(repo_path, "main");
        assert!(
            !ok && stderr.contains("uncommitted changes"),
            "an UPDATE is uncommitted work and must be refused; ok: {ok}, stderr: {stderr}"
        );
    });
}
