//! End-to-end tests for `gfs user` against a real Postgres container.
//!
//! Two scenarios, each on its own fresh repo/container (auto-provisioned and
//! cleaned up by `common::postgres::with_fresh_repo`, which also skips — not
//! fails — when the local `gfs-postgres:17` image is absent):
//!
//! 1. `user_lifecycle_create_rotate_password_drop` — create a login user with a
//!    preset, assert its credential is stored as a fresh SCRAM-SHA-256 hash (not
//!    NULL — the empty-password lockout guard), rotate it with `set-password`
//!    (the stored hash changes), then drop. Auth is verified against the
//!    `pg_authid` catalog, NOT by connecting: the container trusts loopback
//!    (`127.0.0.1` in pg_hba, which is what the in-container exec seam relies
//!    on), so a loopback `psql` ignores the password — client-password
//!    enforcement lives on the external/proxied port, out of scope here.
//! 2. `drop_is_non_destructive_and_recoverable_via_version_control` — the
//!    headline safety property: dropping a user that OWNS a table neither
//!    deletes the table nor its rows (REASSIGN OWNED hands them to the admin),
//!    and the drop is fully reversible through gfs's COW versioning — checking
//!    out the pre-drop commit resurrects the role and its ownership byte-for-byte.
//!
//! macOS-only: `with_fresh_repo` commits via the APFS storage backend, matching
//! the other postgres e2e suites. Docker (or Podman) must be running.
//! **Run with `--test-threads=1`** (shared Docker daemon / host ports).

#![cfg(target_os = "macos")]

mod common;

use std::path::Path;
use std::thread;
use std::time::Duration;

use common::cli_runner::{gfs_checkout, gfs_commit, run_gfs, run_gfs_subprocess};
use common::container_runtime::runtime_command;
use common::postgres::{get_container_id, run_psql_select, with_fresh_repo};
use serial_test::serial;

/// Run `gfs user <args…> --path <repo>` in-process. Returns success only; the
/// authoritative state is always read back from Postgres via `run_psql_select`,
/// so we never depend on the (gag-captured, sometimes flaky) stdout here.
fn gfs_user(repo: &Path, args: &[&str]) -> bool {
    let mut full = vec!["gfs", "user"];
    full.extend_from_slice(args);
    full.extend(["--path", repo.to_str().unwrap()]);
    let (ok, _out, err) = run_gfs(full);
    if !ok {
        eprintln!("gfs user {args:?} failed: {err}");
    }
    ok
}

/// Poll until the container's Postgres accepts a socket connection (post-checkout
/// the container is restarted on the swapped data dir and needs a moment).
fn wait_for_pg(container_id: &str) {
    for _ in 0..30 {
        let up = runtime_command()
            .args([
                "exec",
                container_id,
                "psql",
                "-U",
                "postgres",
                "-c",
                "SELECT 1",
            ])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if up {
            return;
        }
        thread::sleep(Duration::from_secs(1));
    }
    panic!("postgres in {container_id} did not become ready within 30s");
}

/// The stored credential for `user`, read from `pg_authid` as the superuser.
/// A well-formed password is a `SCRAM-SHA-256$…` verifier; a NULL/empty one
/// would render the login unusable. Method-agnostic ground truth (loopback is
/// trust auth, so an actual connection can't distinguish passwords).
fn stored_credential(container_id: &str, user: &str) -> String {
    run_psql_select(
        container_id,
        &format!("SELECT COALESCE(rolpassword,'<null>') FROM pg_authid WHERE rolname='{user}'"),
    )
    .trim()
    .to_string()
}

/// Full-hash of the (first) commit whose message equals `message`, read from
/// `gfs log --json` (subprocess: stdout is reliably captured there).
fn commit_hash_by_message(repo: &Path, message: &str) -> String {
    let (_ok, stdout, _err) =
        run_gfs_subprocess(["gfs", "log", "--path", repo.to_str().unwrap(), "--json"]);
    let value: serde_json::Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("log --json parse ({e}): {stdout}"));
    for commit in value["commits"].as_array().expect("commits array") {
        if commit["message"].as_str() == Some(message) {
            return commit["hash_full"].as_str().expect("hash_full").to_string();
        }
    }
    panic!("no commit with message {message:?} in log: {stdout}");
}

#[test]
#[serial]
fn user_lifecycle_create_rotate_password_drop() {
    with_fresh_repo(|repo| {
        let cid = get_container_id(repo);

        // create a login user with the readwrite preset + a known password.
        assert!(gfs_user(
            repo,
            &[
                "create",
                "app_rw",
                "--preset",
                "readwrite",
                "--password",
                "initial_pw"
            ],
        ));
        assert!(
            run_psql_select(&cid, "SELECT rolname FROM pg_roles WHERE rolname='app_rw'")
                .contains("app_rw"),
            "created role must exist in pg_roles",
        );
        // the client role is never privileged (v2 escalation guard).
        assert!(
            run_psql_select(&cid, "SELECT rolsuper FROM pg_roles WHERE rolname='app_rw'")
                .trim_start()
                .starts_with('f'),
            "client role must be NOSUPERUSER",
        );
        // password stored as a real SCRAM verifier — never NULL (lockout guard).
        let created_hash = stored_credential(&cid, "app_rw");
        assert!(
            created_hash.starts_with("SCRAM-SHA-256$"),
            "password must be stored as a SCRAM-SHA-256 verifier, got: {created_hash}",
        );

        // rotate the password: the stored verifier must change.
        assert!(gfs_user(
            repo,
            &["set-password", "app_rw", "--password", "rotated_pw"],
        ));
        let rotated_hash = stored_credential(&cid, "app_rw");
        assert!(
            rotated_hash.starts_with("SCRAM-SHA-256$"),
            "rotated password must still be a SCRAM verifier, got: {rotated_hash}",
        );
        assert_ne!(
            created_hash, rotated_hash,
            "set-password must change the stored credential",
        );

        // list surfaces the role (and never the private management role).
        assert!(gfs_user(repo, &["list"]));

        // drop removes the role.
        assert!(gfs_user(repo, &["drop", "app_rw"]));
        assert!(
            !run_psql_select(&cid, "SELECT rolname FROM pg_roles WHERE rolname='app_rw'")
                .contains("app_rw"),
            "dropped role must be gone from pg_roles",
        );
    });
}

#[test]
#[serial]
fn drop_is_non_destructive_and_recoverable_via_version_control() {
    with_fresh_repo(|repo| {
        let cid = get_container_id(repo);

        // A user that OWNS a table with data.
        assert!(gfs_user(
            repo,
            &[
                "create",
                "dataowner",
                "--preset",
                "readwrite",
                "--password",
                "pw"
            ],
        ));
        run_psql_select(
            &cid,
            "CREATE TABLE ledger(id int primary key, amount int); \
             ALTER TABLE ledger OWNER TO dataowner; \
             INSERT INTO ledger VALUES (1,100),(2,200),(3,300);",
        );
        assert_eq!(
            run_psql_select(
                &cid,
                "SELECT tableowner FROM pg_tables WHERE tablename='ledger'"
            )
            .trim(),
            "dataowner",
            "table should initially be owned by dataowner",
        );

        // Commit C1 — snapshot WITH the user + its owned table.
        let (ok, _o, err) = gfs_commit(repo, "with-dataowner", Some("t"), Some("t@e.co"));
        assert!(ok, "commit C1 should succeed; stderr: {err}");
        let c1 = commit_hash_by_message(repo, "with-dataowner");

        // Drop the owner. This must NOT destroy the table or its rows:
        // REASSIGN OWNED hands `ledger` to the management role first.
        assert!(gfs_user(repo, &["drop", "dataowner"]));
        assert!(
            !run_psql_select(
                &cid,
                "SELECT rolname FROM pg_roles WHERE rolname='dataowner'"
            )
            .contains("dataowner"),
            "dropped role must be gone",
        );
        assert_eq!(
            run_psql_select(&cid, "SELECT count(*) FROM ledger").trim(),
            "3",
            "dropping the owner must NOT delete its table's rows (no cascade)",
        );
        assert_eq!(
            run_psql_select(
                &cid,
                "SELECT tableowner FROM pg_tables WHERE tablename='ledger'"
            )
            .trim(),
            "postgres",
            "ledger must be reassigned to the management role, not dropped",
        );

        // Commit C2 — snapshot WITHOUT the user.
        let (ok, _o, err) = gfs_commit(repo, "dropped-dataowner", Some("t"), Some("t@e.co"));
        assert!(ok, "commit C2 should succeed; stderr: {err}");

        // Checkout C1 (pre-drop): version control must resurrect the role AND
        // restore the table's original ownership — the drop is fully reversible.
        let (ok, _o, err) = gfs_checkout(repo, &c1);
        assert!(ok, "checkout C1 should succeed; stderr: {err}");
        let cid = get_container_id(repo); // stable across checkout, but re-read to be safe
        wait_for_pg(&cid);
        assert!(
            run_psql_select(
                &cid,
                "SELECT rolname FROM pg_roles WHERE rolname='dataowner'"
            )
            .contains("dataowner"),
            "checking out the pre-drop commit must bring the dropped user back",
        );
        assert_eq!(
            run_psql_select(
                &cid,
                "SELECT tableowner FROM pg_tables WHERE tablename='ledger'"
            )
            .trim(),
            "dataowner",
            "ownership at C1 must be restored to dataowner",
        );
        assert_eq!(
            run_psql_select(&cid, "SELECT count(*) FROM ledger").trim(),
            "3",
            "data intact at C1",
        );

        // Checkout back to main (C2): the user is gone again.
        let (ok, _o, err) = gfs_checkout(repo, "main");
        assert!(ok, "checkout main should succeed; stderr: {err}");
        let cid = get_container_id(repo);
        wait_for_pg(&cid);
        assert!(
            !run_psql_select(
                &cid,
                "SELECT rolname FROM pg_roles WHERE rolname='dataowner'"
            )
            .contains("dataowner"),
            "back on main (post-drop) the user must be absent again",
        );
    });
}

/// Story 007-004: object-level `gfs user grant/revoke/list-privs` change real
/// privileges. Drives the ACTUAL CLI (clap parse → dispatch → run_grant →
/// DockerCompute) and verifies against `has_table_privilege` in the engine.
#[test]
#[serial]
fn user_grant_revoke_list_privileges() {
    with_fresh_repo(|repo| {
        let cid = get_container_id(repo);

        // A least-privileged client role + a table to grant on.
        assert!(gfs_user(
            repo,
            &[
                "create",
                "app_ro",
                "--preset",
                "readonly",
                "--password",
                "pw"
            ],
        ));
        run_psql_select(&cid, "CREATE TABLE public.orders(id int);");

        // Baseline from the preset: `readonly` grants SELECT via membership in the
        // `gfs_readonly` group, but not INSERT. A direct grant/revoke of SELECT would
        // be masked by the group, so exercise the direct object-grant path on INSERT
        // (which `readonly` does not provide) to observe the change unambiguously.
        assert_eq!(
            run_psql_select(
                &cid,
                "SELECT has_table_privilege('app_ro','public.orders','SELECT')"
            )
            .trim(),
            "t",
            "readonly preset grants SELECT via the gfs_readonly group",
        );
        assert_eq!(
            run_psql_select(
                &cid,
                "SELECT has_table_privilege('app_ro','public.orders','INSERT')"
            )
            .trim(),
            "f",
            "readonly must not grant INSERT",
        );

        // GRANT INSERT (a direct object grant) via the CLI.
        assert!(gfs_user(
            repo,
            &[
                "grant",
                "app_ro",
                "--on-table",
                "public.orders",
                "--privileges",
                "INSERT",
            ],
        ));
        assert_eq!(
            run_psql_select(
                &cid,
                "SELECT has_table_privilege('app_ro','public.orders','INSERT')"
            )
            .trim(),
            "t",
            "CLI grant must give app_ro INSERT on public.orders",
        );

        // list-privs runs (authoritative check is the has_table_privilege above).
        assert!(gfs_user(repo, &["list-privs", "app_ro"]));

        // REVOKE INSERT (the direct grant) via the CLI.
        assert!(gfs_user(
            repo,
            &[
                "revoke",
                "app_ro",
                "--on-table",
                "public.orders",
                "--privileges",
                "INSERT",
            ],
        ));
        assert_eq!(
            run_psql_select(
                &cid,
                "SELECT has_table_privilege('app_ro','public.orders','INSERT')"
            )
            .trim(),
            "f",
            "CLI revoke must remove the directly-granted INSERT",
        );
        // The group-provided SELECT is untouched by a direct object revoke.
        assert_eq!(
            run_psql_select(
                &cid,
                "SELECT has_table_privilege('app_ro','public.orders','SELECT')"
            )
            .trim(),
            "t",
            "the group-provided SELECT survives a direct revoke",
        );
    });
}
