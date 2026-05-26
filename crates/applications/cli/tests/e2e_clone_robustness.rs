//! Reproduction + regression tests for `gfs clone` robustness (RFC 008).
//!
//! Two issues these tests pin down:
//!   1. The clone must provision the local engine with the **same major version**
//!      as the remote (not a hardcoded default).
//!   2. The clone must **tolerate source extensions**: a table using an
//!      extension type absent locally (e.g. pgvector's `vector`) must not abort
//!      the whole clone — such tables are skipped, the rest clone.
//!
//! Plus copy-on-write overlay invariants: write semantics on a single-column PK
//! (override without duplication, delete/tombstone/revive, PK-changing update),
//! full CRUD on a composite PK, and mirroring of server-side column defaults.
//!
//! macOS-only (consistent with the other e2e suites); Docker/Podman required;
//! relies on Docker Desktop's `host.docker.internal`.

#![cfg(target_os = "macos")]

mod common;

use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

use common::container_runtime::runtime_command;
use serial_test::serial;
use tempfile::TempDir;

/// Removes any registered containers and the repo on drop.
struct Cleanup {
    containers: Vec<String>,
    repo: Option<TempDir>,
}
impl Cleanup {
    fn new(repo: TempDir) -> Self {
        Cleanup { containers: Vec::new(), repo: Some(repo) }
    }
    fn add(&mut self, name: impl Into<String>) {
        self.containers.push(name.into());
    }
}
impl Drop for Cleanup {
    fn drop(&mut self) {
        for c in &self.containers {
            let _ = runtime_command().args(["rm", "-f", c]).output();
        }
        drop(self.repo.take());
    }
}

fn psql(container: &str, db: &str, query: &str) -> String {
    let out = runtime_command()
        .args(["exec", container, "psql", "-U", "postgres", "-d", db, "-tAc", query])
        .output()
        .expect("psql exec");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// Start a remote postgres from `image` with a Docker-assigned host port and
/// wait for readiness. Returns the mapped host port.
fn start_remote(name: &str, image: &str) -> String {
    let _ = runtime_command().args(["rm", "-f", name]).output();
    let started = runtime_command()
        .args([
            "run", "-d", "--name", name,
            "-e", "POSTGRES_PASSWORD=postgres",
            "-e", "POSTGRES_DB=shop",
            "-p", "127.0.0.1::5432",
            image,
        ])
        .output()
        .expect("start remote");
    assert!(started.status.success(), "start {image}: {}", String::from_utf8_lossy(&started.stderr));

    let port_out = runtime_command().args(["port", name, "5432"]).output().expect("docker port");
    let mapped = String::from_utf8_lossy(&port_out.stdout);
    let host_port = mapped
        .lines().next().and_then(|l| l.rsplit(':').next())
        .map(|s| s.trim().to_string())
        .expect("mapped port");

    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let ready = runtime_command()
            .args(["exec", name, "pg_isready", "-U", "postgres", "-d", "shop"])
            .output().map(|o| o.status.success()).unwrap_or(false);
        if ready { break; }
        assert!(Instant::now() < deadline, "{name} never ready");
        thread::sleep(Duration::from_millis(500));
    }
    host_port
}

/// Like `psql` but returns the raw Output so callers can assert on failure.
/// `ON_ERROR_STOP=1` makes a failing statement yield a non-zero exit code.
fn psql_try(container: &str, db: &str, query: &str) -> std::process::Output {
    runtime_command()
        .args(["exec", container, "psql", "-U", "postgres", "-d", db, "-v", "ON_ERROR_STOP=1", "-tAc", query])
        .output()
        .expect("psql exec")
}

fn seed_remote(name: &str, sql: &str) {
    let out = runtime_command()
        .args(["exec", name, "psql", "-U", "postgres", "-d", "shop", "-v", "ON_ERROR_STOP=1", "-c", sql])
        .output().expect("seed");
    assert!(out.status.success(), "seed failed: {}", String::from_utf8_lossy(&out.stderr));
}

const READER: &str =
    "DO $$ BEGIN IF NOT EXISTS (SELECT FROM pg_roles WHERE rolname='gfs_reader') \
     THEN CREATE ROLE gfs_reader LOGIN PASSWORD 'readerpw'; END IF; END $$; ";

fn grant_reader() -> &'static str {
    "GRANT USAGE ON SCHEMA public TO gfs_reader; \
     GRANT SELECT ON ALL TABLES IN SCHEMA public TO gfs_reader;"
}

fn run_clone(url: &str, repo: &std::path::Path, version: Option<&str>) -> std::process::Output {
    let mut args = vec!["clone".to_string(), "--from".to_string(), url.to_string(), repo.to_str().unwrap().to_string()];
    if let Some(v) = version {
        args.push("--database-version".into());
        args.push(v.into());
    }
    Command::new(env!("CARGO_BIN_EXE_gfs")).args(&args).output().expect("run gfs clone")
}

/// Issue 1: the local clone must run the same major version as the remote.
#[test]
#[serial]
fn clone_matches_remote_major_version() {
    let repo = TempDir::new().unwrap();
    let repo_path = repo.path().to_path_buf();
    let mut cleanup = Cleanup::new(repo);

    let remote = "gfs-e2e-ver-remote";
    cleanup.add(remote);
    let port = start_remote(remote, "postgres:16");
    seed_remote(remote, &format!(
        "{READER} CREATE TABLE orders (id bigint PRIMARY KEY, name text NOT NULL); \
         INSERT INTO orders SELECT g,'n'||g FROM generate_series(1,10) g; {}", grant_reader()));
    let remote_major = psql(remote, "shop", "SHOW server_version_num")[..2].to_string();

    let url = format!("postgres://gfs_reader:readerpw@host.docker.internal:{port}/shop");
    // No --database-version → must be inferred from the remote.
    let out = run_clone(&url, &repo_path, None);
    assert!(out.status.success(), "clone failed: {}", String::from_utf8_lossy(&out.stderr));

    let gfs = common::postgres::get_container_id(&repo_path);
    cleanup.add(gfs.clone());
    let local_major = psql(&gfs, "postgres", "SHOW server_version_num")[..2].to_string();

    assert_eq!(
        local_major, remote_major,
        "clone provisioned major {local_major} but remote is {remote_major}"
    );
}

/// Issue 2: a source extension type (pgvector `vector`) must not abort the clone.
#[test]
#[serial]
fn clone_tolerates_source_extensions() {
    let repo = TempDir::new().unwrap();
    let repo_path = repo.path().to_path_buf();
    let mut cleanup = Cleanup::new(repo);

    let remote = "gfs-e2e-ext-remote";
    cleanup.add(remote);
    let port = start_remote(remote, "pgvector/pgvector:pg16");
    seed_remote(remote, &format!(
        "{READER} CREATE EXTENSION IF NOT EXISTS vector; \
         CREATE TABLE products (id bigint PRIMARY KEY, embedding vector(3)); \
         INSERT INTO products VALUES (1,'[1,2,3]'),(2,'[4,5,6]'); \
         CREATE TABLE categories (id bigint PRIMARY KEY, name text NOT NULL); \
         INSERT INTO categories SELECT g,'cat_'||g FROM generate_series(1,5) g; {}", grant_reader()));

    let url = format!("postgres://gfs_reader:readerpw@host.docker.internal:{port}/shop");
    // Pin version 16 to isolate the extension issue from the version issue.
    let out = run_clone(&url, &repo_path, Some("16"));
    assert!(
        out.status.success(),
        "clone aborted on a source extension type:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    let gfs = common::postgres::get_container_id(&repo_path);
    cleanup.add(gfs.clone());
    // The plain table must have cloned (extension-typed one may be skipped).
    assert_eq!(psql(&gfs, "postgres", "SELECT count(*) FROM categories"), "5");
}

/// `gfs clone --image` provisions an image that bundles the source's extension,
/// so the extension-typed table clones fully (no skip).
#[test]
#[serial]
fn clone_with_image_supports_extension() {
    let repo = TempDir::new().unwrap();
    let repo_path = repo.path().to_path_buf();
    let mut cleanup = Cleanup::new(repo);

    let remote = "gfs-e2e-img-remote";
    cleanup.add(remote);
    let port = start_remote(remote, "pgvector/pgvector:pg16");
    seed_remote(remote, &format!(
        "{READER} CREATE EXTENSION IF NOT EXISTS vector; \
         CREATE TABLE products (id bigint PRIMARY KEY, embedding vector(3)); \
         INSERT INTO products VALUES (1,'[1,2,3]'),(2,'[4,5,6]'); {}", grant_reader()));

    let url = format!("postgres://gfs_reader:readerpw@host.docker.internal:{port}/shop");
    let out = Command::new(env!("CARGO_BIN_EXE_gfs"))
        .args([
            "clone", "--from", &url, repo_path.to_str().unwrap(),
            "--image", "pgvector/pgvector:pg16",
        ])
        .output()
        .expect("run gfs clone --image");
    assert!(
        out.status.success(),
        "clone --image failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    let gfs = common::postgres::get_container_id(&repo_path);
    cleanup.add(gfs.clone());
    // With the right image, the vector-typed table clones fully (not skipped).
    assert_eq!(
        psql(&gfs, "postgres", "SELECT relkind FROM pg_class WHERE relname='products' AND relnamespace='public'::regnamespace"),
        "v",
        "products should be an overlay view when --image provides pgvector"
    );
    assert_eq!(psql(&gfs, "postgres", "SELECT count(*) FROM products"), "2");
}

/// `gfs clone --platform` must provision the local container for that platform
/// (so an image lacking a native-arch manifest can run under emulation).
#[test]
#[serial]
fn clone_honors_platform_override() {
    let repo = TempDir::new().unwrap();
    let repo_path = repo.path().to_path_buf();
    let mut cleanup = Cleanup::new(repo);

    let remote = "gfs-e2e-plat-remote";
    cleanup.add(remote);
    let port = start_remote(remote, "postgres:16");
    seed_remote(remote, &format!(
        "{READER} CREATE TABLE t (id bigint PRIMARY KEY); \
         INSERT INTO t SELECT generate_series(1,5); {}", grant_reader()));

    let url = format!("postgres://gfs_reader:readerpw@host.docker.internal:{port}/shop");
    let out = Command::new(env!("CARGO_BIN_EXE_gfs"))
        .args([
            "clone", "--from", &url, repo_path.to_str().unwrap(),
            "--image", "postgres:16", "--platform", "linux/amd64",
        ])
        .output()
        .expect("run gfs clone --platform");
    assert!(
        out.status.success(),
        "clone --platform failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    let gfs = common::postgres::get_container_id(&repo_path);
    cleanup.add(gfs.clone());

    // linux/amd64 → the container's userspace is x86_64 regardless of host arch.
    let arch = runtime_command()
        .args(["exec", &gfs, "uname", "-m"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();
    assert_eq!(arch, "x86_64", "container should run as linux/amd64");

    // And the overlay still works through the emulated engine.
    assert_eq!(psql(&gfs, "postgres", "SELECT count(*) FROM t"), "5");
}

/// Remediations: auto-increment works locally (offset past the remote max, no
/// PK collision), and any local role can read through the overlay (FOR PUBLIC).
#[test]
#[serial]
fn clone_autoincrement_and_any_role() {
    let repo = TempDir::new().unwrap();
    let repo_path = repo.path().to_path_buf();
    let mut cleanup = Cleanup::new(repo);

    let remote = "gfs-e2e-seq-remote";
    cleanup.add(remote);
    let port = start_remote(remote, "postgres:16");
    seed_remote(remote, &format!(
        "{READER} CREATE TABLE orders (id bigserial PRIMARY KEY, name text NOT NULL); \
         INSERT INTO orders (name) SELECT 'n'||g FROM generate_series(1,1000) g; {}", grant_reader()));

    let url = format!("postgres://gfs_reader:readerpw@host.docker.internal:{port}/shop");
    let out = Command::new(env!("CARGO_BIN_EXE_gfs"))
        .args(["clone", "--from", &url, repo_path.to_str().unwrap()])
        .output()
        .expect("run gfs clone");
    assert!(out.status.success(), "clone failed: {}", String::from_utf8_lossy(&out.stderr));

    let gfs = common::postgres::get_container_id(&repo_path);
    cleanup.add(gfs.clone());

    // Auto-increment INSERT omitting the PK → first local id is past the remote max.
    psql(&gfs, "postgres", "INSERT INTO orders (name) VALUES ('local')");
    assert_eq!(
        psql(&gfs, "postgres", "SELECT id FROM orders WHERE name='local'"),
        "1001",
        "local sequence should start just past the remote max (1000)"
    );
    assert_eq!(psql(&gfs, "postgres", "SELECT count(*) FROM orders"), "1001");

    // FOR PUBLIC: a different local role can read through the FDW overlay.
    psql(&gfs, "postgres", "CREATE ROLE app LOGIN SUPERUSER");
    let as_app = psql(&gfs, "postgres", "SET ROLE app; SELECT count(*) FROM orders");
    assert!(
        as_app.contains("1001"),
        "any local role should read through the overlay (user mapping FOR PUBLIC), got: {as_app}"
    );
}

/// `gfs clone --port` must bind the local engine to the requested host port.
#[test]
#[serial]
fn clone_binds_requested_port() {
    let repo = TempDir::new().unwrap();
    let repo_path = repo.path().to_path_buf();
    let mut cleanup = Cleanup::new(repo);

    let remote = "gfs-e2e-port-remote";
    cleanup.add(remote);
    let port = start_remote(remote, "postgres:17");
    seed_remote(remote, &format!(
        "{READER} CREATE TABLE t (id bigint PRIMARY KEY); \
         INSERT INTO t SELECT generate_series(1,5); {}", grant_reader()));

    let url = format!("postgres://gfs_reader:readerpw@host.docker.internal:{port}/shop");
    let chosen = "64217";
    let out = Command::new(env!("CARGO_BIN_EXE_gfs"))
        .args([
            "clone", "--from", &url, repo_path.to_str().unwrap(),
            "--port", chosen, "--database-version", "17",
        ])
        .output()
        .expect("run gfs clone --port");
    assert!(
        out.status.success(),
        "clone --port failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    let gfs = common::postgres::get_container_id(&repo_path);
    cleanup.add(gfs.clone());

    let bind = runtime_command()
        .args(["port", &gfs, "5432"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
        .unwrap_or_default();
    assert!(
        bind.contains(chosen),
        "local container should bind requested port {chosen}, got: {bind}"
    );
}

/// Copy-on-write overlay invariants on a single-column PK, all against one
/// clone: a local INSERT/UPDATE on a remote key wins without duplicating the
/// row (UNION ALL stays disjoint); DELETE tombstones the remote row and a
/// re-INSERT of the same key revives it; an UPDATE that changes the PK
/// tombstones the old key and exposes the new one. The remote stays read-only.
#[test]
#[serial]
fn clone_overlay_write_semantics() {
    let repo = TempDir::new().unwrap();
    let repo_path = repo.path().to_path_buf();
    let mut cleanup = Cleanup::new(repo);

    let remote = "gfs-e2e-overlay-remote";
    cleanup.add(remote);
    let port = start_remote(remote, "postgres:16");
    seed_remote(remote, &format!(
        "{READER} CREATE TABLE orders (id bigint PRIMARY KEY, name text NOT NULL); \
         INSERT INTO orders SELECT g,'n'||g FROM generate_series(1,100) g; {}", grant_reader()));

    let url = format!("postgres://gfs_reader:readerpw@host.docker.internal:{port}/shop");
    let out = run_clone(&url, &repo_path, Some("16"));
    assert!(out.status.success(), "clone failed: {}", String::from_utf8_lossy(&out.stderr));

    let gfs = common::postgres::get_container_id(&repo_path);
    cleanup.add(gfs.clone());

    // Baseline: the overlay reads exactly the remote.
    assert_eq!(psql(&gfs, "postgres", "SELECT count(*) FROM orders"), "100");

    // INSERT a key that already exists remotely → local wins, no duplicate row.
    psql(&gfs, "postgres", "INSERT INTO orders (id,name) VALUES (5,'LOCAL5')");
    assert_eq!(psql(&gfs, "postgres", "SELECT name FROM orders WHERE id=5"), "LOCAL5");
    assert_eq!(
        psql(&gfs, "postgres", "SELECT count(*) FROM orders"),
        "100",
        "overriding a remote row must not duplicate it (UNION ALL disjoint)"
    );

    // UPDATE a remote-only row → copy-on-write into the local store, still no dup.
    psql(&gfs, "postgres", "UPDATE orders SET name='UPD7' WHERE id=7");
    assert_eq!(psql(&gfs, "postgres", "SELECT name FROM orders WHERE id=7"), "UPD7");
    assert_eq!(psql(&gfs, "postgres", "SELECT count(*) FROM orders"), "100");

    // DELETE a remote row → tombstoned, hidden from the view.
    psql(&gfs, "postgres", "DELETE FROM orders WHERE id=9");
    assert_eq!(psql(&gfs, "postgres", "SELECT count(*) FROM orders WHERE id=9"), "0");
    assert_eq!(psql(&gfs, "postgres", "SELECT count(*) FROM orders"), "99");

    // Re-INSERT the same key → tombstone cleared, row revived.
    psql(&gfs, "postgres", "INSERT INTO orders (id,name) VALUES (9,'REBORN')");
    assert_eq!(psql(&gfs, "postgres", "SELECT name FROM orders WHERE id=9"), "REBORN");
    assert_eq!(psql(&gfs, "postgres", "SELECT count(*) FROM orders"), "100");

    // UPDATE that CHANGES the PK → old key tombstoned, new key exposed.
    psql(&gfs, "postgres", "UPDATE orders SET id=100001 WHERE id=11");
    assert_eq!(
        psql(&gfs, "postgres", "SELECT count(*) FROM orders WHERE id=11"),
        "0",
        "old key must be hidden after a PK-changing update"
    );
    assert_eq!(
        psql(&gfs, "postgres", "SELECT name FROM orders WHERE id=100001"),
        "n11",
        "new key must carry the row"
    );
    assert_eq!(psql(&gfs, "postgres", "SELECT count(*) FROM orders"), "100");

    // The remote was never written to.
    assert_eq!(psql(remote, "shop", "SELECT count(*) FROM orders"), "100");
    assert_eq!(psql(remote, "shop", "SELECT name FROM orders WHERE id=5"), "n5");
    assert_eq!(psql(remote, "shop", "SELECT name FROM orders WHERE id=9"), "n9");
    assert_eq!(psql(remote, "shop", "SELECT count(*) FROM orders WHERE id=100001"), "0");
}

/// The overlay supports full CRUD on a table whose primary key is composite:
/// read, update (no dup), insert, delete (tombstone) and re-insert of the same
/// composite key. The remote stays untouched.
#[test]
#[serial]
fn clone_overlay_composite_key_crud() {
    let repo = TempDir::new().unwrap();
    let repo_path = repo.path().to_path_buf();
    let mut cleanup = Cleanup::new(repo);

    let remote = "gfs-e2e-composite-remote";
    cleanup.add(remote);
    let port = start_remote(remote, "postgres:16");
    seed_remote(remote, &format!(
        "{READER} CREATE TABLE order_items ( \
            order_id bigint NOT NULL, line_no int NOT NULL, qty int NOT NULL, \
            PRIMARY KEY (order_id, line_no)); \
         INSERT INTO order_items VALUES (1,1,10),(1,2,20),(2,1,30); {}", grant_reader()));

    let url = format!("postgres://gfs_reader:readerpw@host.docker.internal:{port}/shop");
    let out = run_clone(&url, &repo_path, Some("16"));
    assert!(out.status.success(), "clone failed: {}", String::from_utf8_lossy(&out.stderr));

    let gfs = common::postgres::get_container_id(&repo_path);
    cleanup.add(gfs.clone());

    // The composite-key table is registered and exposed as an overlay view.
    assert_eq!(
        psql(&gfs, "postgres", "SELECT relkind FROM pg_class WHERE relname='order_items' AND relnamespace='public'::regnamespace"),
        "v"
    );
    assert_eq!(psql(&gfs, "postgres", "SELECT count(*) FROM order_items"), "3");
    assert_eq!(
        psql(&gfs, "postgres", "SELECT qty FROM order_items WHERE order_id=1 AND line_no=2"),
        "20"
    );

    // UPDATE on a composite key → copy-on-write, no duplicate.
    psql(&gfs, "postgres", "UPDATE order_items SET qty=99 WHERE order_id=1 AND line_no=1");
    assert_eq!(
        psql(&gfs, "postgres", "SELECT qty FROM order_items WHERE order_id=1 AND line_no=1"),
        "99"
    );
    assert_eq!(psql(&gfs, "postgres", "SELECT count(*) FROM order_items"), "3");

    // INSERT a new composite key.
    psql(&gfs, "postgres", "INSERT INTO order_items VALUES (3,1,40)");
    assert_eq!(psql(&gfs, "postgres", "SELECT count(*) FROM order_items"), "4");

    // DELETE then re-INSERT the same composite key (tombstone then revive).
    psql(&gfs, "postgres", "DELETE FROM order_items WHERE order_id=2 AND line_no=1");
    assert_eq!(psql(&gfs, "postgres", "SELECT count(*) FROM order_items"), "3");
    psql(&gfs, "postgres", "INSERT INTO order_items VALUES (2,1,77)");
    assert_eq!(psql(&gfs, "postgres", "SELECT count(*) FROM order_items"), "4");
    assert_eq!(
        psql(&gfs, "postgres", "SELECT qty FROM order_items WHERE order_id=2 AND line_no=1"),
        "77"
    );

    // Remote untouched.
    assert_eq!(psql(remote, "shop", "SELECT count(*) FROM order_items"), "3");
    assert_eq!(
        psql(remote, "shop", "SELECT qty FROM order_items WHERE order_id=1 AND line_no=1"),
        "10"
    );
}

/// Server-side column DEFAULTs (now(), uuid_generate_v4(), constants) are
/// dropped by IMPORT FOREIGN SCHEMA, so the clone must mirror them onto the
/// overlay view; otherwise an app that omits a NOT NULL DEFAULT column would
/// insert NULL and fail. Inserting through the overlay while omitting every
/// defaulted column must succeed and pick up the defaults.
#[test]
#[serial]
fn clone_mirrors_column_defaults() {
    let repo = TempDir::new().unwrap();
    let repo_path = repo.path().to_path_buf();
    let mut cleanup = Cleanup::new(repo);

    let remote = "gfs-e2e-defaults-remote";
    cleanup.add(remote);
    let port = start_remote(remote, "postgres:16");
    seed_remote(remote, &format!(
        "{READER} CREATE EXTENSION IF NOT EXISTS \"uuid-ossp\"; \
         CREATE TABLE events ( \
            id uuid PRIMARY KEY DEFAULT uuid_generate_v4(), \
            kind text NOT NULL DEFAULT 'generic', \
            created_at timestamptz NOT NULL DEFAULT now(), \
            payload int NOT NULL DEFAULT 0); \
         INSERT INTO events (kind) VALUES ('seed1'),('seed2'); {}", grant_reader()));

    let url = format!("postgres://gfs_reader:readerpw@host.docker.internal:{port}/shop");
    let out = run_clone(&url, &repo_path, Some("16"));
    assert!(out.status.success(), "clone failed: {}", String::from_utf8_lossy(&out.stderr));

    let gfs = common::postgres::get_container_id(&repo_path);
    cleanup.add(gfs.clone());

    assert_eq!(psql(&gfs, "postgres", "SELECT count(*) FROM events"), "2");

    // INSERT omitting id / created_at / payload (all NOT NULL with a default):
    // without mirrored defaults this fails with a not-null violation.
    psql(&gfs, "postgres", "INSERT INTO events (kind) VALUES ('app1')");
    assert_eq!(psql(&gfs, "postgres", "SELECT count(*) FROM events"), "3");
    assert_eq!(
        psql(&gfs, "postgres",
            "SELECT count(*) FROM events WHERE kind='app1' \
             AND id IS NOT NULL AND created_at IS NOT NULL AND payload=0"),
        "1",
        "mirrored defaults (uuid_generate_v4(), now(), 0) must populate the omitted columns"
    );

    // The remote is unchanged.
    assert_eq!(psql(remote, "shop", "SELECT count(*) FROM events"), "2");
}

/// A remote table with no primary key and no unique index cannot get an
/// updatable overlay, so it is skipped (no public view, not in the sync
/// catalog) — but the clone still succeeds and keyed tables clone normally.
#[test]
#[serial]
fn clone_skips_keyless_table() {
    let repo = TempDir::new().unwrap();
    let repo_path = repo.path().to_path_buf();
    let mut cleanup = Cleanup::new(repo);

    let remote = "gfs-e2e-keyless-remote";
    cleanup.add(remote);
    let port = start_remote(remote, "postgres:16");
    seed_remote(remote, &format!(
        "{READER} \
         CREATE TABLE logs (msg text NOT NULL, ts timestamptz); \
         INSERT INTO logs SELECT 'm'||g, now() FROM generate_series(1,5) g; \
         CREATE TABLE orders (id bigint PRIMARY KEY, name text NOT NULL); \
         INSERT INTO orders SELECT g,'n'||g FROM generate_series(1,10) g; {}", grant_reader()));

    let url = format!("postgres://gfs_reader:readerpw@host.docker.internal:{port}/shop");
    let out = run_clone(&url, &repo_path, Some("16"));
    assert!(
        out.status.success(),
        "clone must succeed despite a keyless table:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    let gfs = common::postgres::get_container_id(&repo_path);
    cleanup.add(gfs.clone());

    // The keyed table cloned: overlay view + registered + correct count.
    assert_eq!(
        psql(&gfs, "postgres", "SELECT relkind FROM pg_class WHERE relname='orders' AND relnamespace='public'::regnamespace"),
        "v"
    );
    assert_eq!(psql(&gfs, "postgres", "SELECT count(*) FROM orders"), "10");
    assert_eq!(
        psql(&gfs, "postgres", "SELECT count(*) FROM gfs_sync.table_meta WHERE table_name='orders'"),
        "1"
    );

    // The keyless table has no overlay in public and is not registered.
    assert_eq!(
        psql(&gfs, "postgres", "SELECT count(*) FROM pg_class WHERE relname='logs' AND relnamespace='public'::regnamespace"),
        "0",
        "keyless table must not be exposed as a public overlay"
    );
    assert_eq!(
        psql(&gfs, "postgres", "SELECT count(*) FROM gfs_sync.table_meta WHERE table_name='logs'"),
        "0",
        "keyless table must not be in the sync catalog"
    );
}

/// The clone is copy-on-read live federation: when the remote is down, reads
/// through the overlay view fail (the foreign branch can't connect) rather than
/// returning wrong data — while locally-owned rows (written before the outage)
/// survive in the authoritative local store.
#[test]
#[serial]
fn clone_reads_fail_gracefully_when_remote_down() {
    let repo = TempDir::new().unwrap();
    let repo_path = repo.path().to_path_buf();
    let mut cleanup = Cleanup::new(repo);

    let remote = "gfs-e2e-down-remote";
    cleanup.add(remote);
    let port = start_remote(remote, "postgres:16");
    seed_remote(remote, &format!(
        "{READER} CREATE TABLE orders (id bigint PRIMARY KEY, name text NOT NULL); \
         INSERT INTO orders SELECT g,'n'||g FROM generate_series(1,10) g; {}", grant_reader()));

    let url = format!("postgres://gfs_reader:readerpw@host.docker.internal:{port}/shop");
    let out = run_clone(&url, &repo_path, Some("16"));
    assert!(out.status.success(), "clone failed: {}", String::from_utf8_lossy(&out.stderr));

    let gfs = common::postgres::get_container_id(&repo_path);
    cleanup.add(gfs.clone());

    // Remote up: read works, and a local write takes ownership of one row.
    assert_eq!(psql(&gfs, "postgres", "SELECT count(*) FROM orders"), "10");
    psql(&gfs, "postgres", "UPDATE orders SET name='LOCAL1' WHERE id=1");

    // Take the remote down.
    let stopped = runtime_command().args(["stop", remote]).output().expect("stop remote");
    assert!(stopped.status.success(), "could not stop remote: {}", String::from_utf8_lossy(&stopped.stderr));

    // A read through the overlay now fails (foreign branch can't connect) —
    // it must error, not silently drop the remote rows.
    let read = psql_try(&gfs, "postgres", "SELECT count(*) FROM orders");
    assert!(
        !read.status.success(),
        "overlay read should fail while the remote is down, got stdout: {}",
        String::from_utf8_lossy(&read.stdout)
    );

    // The authoritative local store survives the outage with the diverged value.
    assert_eq!(
        psql(&gfs, "postgres", "SELECT name FROM orders_local WHERE id=1"),
        "LOCAL1",
        "locally-owned rows must remain readable when the remote is gone"
    );
    assert_eq!(psql(&gfs, "postgres", "SELECT count(*) FROM orders_local"), "1");
}

/// Built-in exotic column types (arrays, jsonb, numeric, inet, uuid, bytea,
/// timestamptz) round-trip through the overlay on both read and write.
#[test]
#[serial]
fn clone_roundtrips_exotic_types() {
    let repo = TempDir::new().unwrap();
    let repo_path = repo.path().to_path_buf();
    let mut cleanup = Cleanup::new(repo);

    let remote = "gfs-e2e-types-remote";
    cleanup.add(remote);
    let port = start_remote(remote, "postgres:16");
    seed_remote(remote, &format!(
        "{READER} CREATE TABLE items ( \
            id bigint PRIMARY KEY, \
            tags text[] NOT NULL, \
            meta jsonb NOT NULL, \
            price numeric(10,2) NOT NULL, \
            ip inet, \
            ref uuid, \
            blob bytea, \
            created_at timestamptz NOT NULL DEFAULT now()); \
         INSERT INTO items (id,tags,meta,price,ip,ref,blob) VALUES \
            (1, ARRAY['a','b'], '{{\"k\":1}}', 9.99, '10.0.0.1', \
             '11111111-1111-1111-1111-111111111111', '\\xdeadbeef'), \
            (2, ARRAY['c'], '{{\"k\":2,\"n\":[1,2]}}', 1.50, NULL, NULL, NULL); {}",
        grant_reader()));

    let url = format!("postgres://gfs_reader:readerpw@host.docker.internal:{port}/shop");
    let out = run_clone(&url, &repo_path, Some("16"));
    assert!(out.status.success(), "clone failed: {}", String::from_utf8_lossy(&out.stderr));

    let gfs = common::postgres::get_container_id(&repo_path);
    cleanup.add(gfs.clone());

    // Read-through fidelity for each type.
    assert_eq!(psql(&gfs, "postgres", "SELECT count(*) FROM items"), "2");
    assert_eq!(psql(&gfs, "postgres", "SELECT tags FROM items WHERE id=1"), "{a,b}");
    assert_eq!(psql(&gfs, "postgres", "SELECT meta->>'k' FROM items WHERE id=1"), "1");
    assert_eq!(psql(&gfs, "postgres", "SELECT (meta->'n')->>1 FROM items WHERE id=2"), "2");
    assert_eq!(psql(&gfs, "postgres", "SELECT price FROM items WHERE id=1"), "9.99");
    assert_eq!(psql(&gfs, "postgres", "SELECT host(ip) FROM items WHERE id=1"), "10.0.0.1");
    assert_eq!(
        psql(&gfs, "postgres", "SELECT ref FROM items WHERE id=1"),
        "11111111-1111-1111-1111-111111111111"
    );
    assert_eq!(psql(&gfs, "postgres", "SELECT encode(blob,'hex') FROM items WHERE id=1"), "deadbeef");

    // Write-through with exotic types (created_at omitted → mirrored default).
    psql(&gfs, "postgres",
        "INSERT INTO items (id,tags,meta,price,blob) \
         VALUES (3, ARRAY['x','y','z'], '{\"w\":true}', 5.00, '\\xcafe')");
    assert_eq!(psql(&gfs, "postgres", "SELECT count(*) FROM items"), "3");
    assert_eq!(psql(&gfs, "postgres", "SELECT array_length(tags,1) FROM items WHERE id=3"), "3");
    assert_eq!(psql(&gfs, "postgres", "SELECT meta->>'w' FROM items WHERE id=3"), "true");
    assert_eq!(psql(&gfs, "postgres", "SELECT encode(blob,'hex') FROM items WHERE id=3"), "cafe");
    assert_eq!(
        psql(&gfs, "postgres", "SELECT created_at IS NOT NULL FROM items WHERE id=3"),
        "t"
    );

    // UPDATE a jsonb value through the overlay.
    psql(&gfs, "postgres", "UPDATE items SET meta='{\"w\":false}' WHERE id=3");
    assert_eq!(psql(&gfs, "postgres", "SELECT meta->>'w' FROM items WHERE id=3"), "false");
    assert_eq!(psql(&gfs, "postgres", "SELECT count(*) FROM items"), "3");

    // Remote untouched.
    assert_eq!(psql(remote, "shop", "SELECT count(*) FROM items"), "2");
}

/// With no schema filter, every non-system schema is mirrored: tables in named
/// schemas become overlays in their own schema (no collision across schemas).
#[test]
#[serial]
fn clone_mirrors_all_schemas() {
    let repo = TempDir::new().unwrap();
    let repo_path = repo.path().to_path_buf();
    let mut cleanup = Cleanup::new(repo);

    let remote = "gfs-e2e-schemas-remote";
    cleanup.add(remote);
    let port = start_remote(remote, "postgres:16");
    seed_remote(remote, &format!(
        "{READER} CREATE SCHEMA sales; CREATE SCHEMA hr; \
         CREATE TABLE sales.account (id bigint PRIMARY KEY, name text NOT NULL); \
         INSERT INTO sales.account VALUES (1,'acme'),(2,'globex'); \
         CREATE TABLE hr.account (id bigint PRIMARY KEY, name text NOT NULL); \
         INSERT INTO hr.account VALUES (1,'alice'); \
         GRANT USAGE ON SCHEMA sales, hr TO gfs_reader; \
         GRANT SELECT ON ALL TABLES IN SCHEMA sales, hr TO gfs_reader; {}", grant_reader()));

    let url = format!("postgres://gfs_reader:readerpw@host.docker.internal:{port}/shop");
    let out = run_clone(&url, &repo_path, Some("16"));
    assert!(out.status.success(), "clone failed: {}", String::from_utf8_lossy(&out.stderr));

    let gfs = common::postgres::get_container_id(&repo_path);
    cleanup.add(gfs.clone());

    // Same table name in two schemas → distinct overlays, no collision.
    assert_eq!(
        psql(&gfs, "postgres", "SELECT relkind FROM pg_class WHERE relname='account' AND relnamespace='sales'::regnamespace"),
        "v"
    );
    assert_eq!(
        psql(&gfs, "postgres", "SELECT relkind FROM pg_class WHERE relname='account' AND relnamespace='hr'::regnamespace"),
        "v"
    );
    assert_eq!(psql(&gfs, "postgres", "SELECT count(*) FROM sales.account"), "2");
    assert_eq!(psql(&gfs, "postgres", "SELECT count(*) FROM hr.account"), "1");
    assert_eq!(
        psql(&gfs, "postgres", "SELECT count(DISTINCT schema_name) FROM gfs_sync.table_meta WHERE table_name='account'"),
        "2"
    );

    // Write into one schema's overlay; the other and the remote are unaffected.
    psql(&gfs, "postgres", "INSERT INTO sales.account VALUES (3,'initech')");
    assert_eq!(psql(&gfs, "postgres", "SELECT count(*) FROM sales.account"), "3");
    assert_eq!(psql(&gfs, "postgres", "SELECT count(*) FROM hr.account"), "1");
    assert_eq!(psql(remote, "shop", "SELECT count(*) FROM sales.account"), "2");
}

/// `?schema=sales` mirrors only the requested schema; others are not created.
#[test]
#[serial]
fn clone_honors_schema_filter() {
    let repo = TempDir::new().unwrap();
    let repo_path = repo.path().to_path_buf();
    let mut cleanup = Cleanup::new(repo);

    let remote = "gfs-e2e-schemafilter-remote";
    cleanup.add(remote);
    let port = start_remote(remote, "postgres:16");
    seed_remote(remote, &format!(
        "{READER} CREATE SCHEMA sales; CREATE SCHEMA hr; \
         CREATE TABLE sales.account (id bigint PRIMARY KEY, name text NOT NULL); \
         INSERT INTO sales.account VALUES (1,'acme'),(2,'globex'); \
         CREATE TABLE hr.account (id bigint PRIMARY KEY, name text NOT NULL); \
         INSERT INTO hr.account VALUES (1,'alice'); \
         GRANT USAGE ON SCHEMA sales, hr TO gfs_reader; \
         GRANT SELECT ON ALL TABLES IN SCHEMA sales, hr TO gfs_reader; {}", grant_reader()));

    let url = format!("postgres://gfs_reader:readerpw@host.docker.internal:{port}/shop?schema=sales");
    let out = run_clone(&url, &repo_path, Some("16"));
    assert!(out.status.success(), "clone failed: {}", String::from_utf8_lossy(&out.stderr));

    let gfs = common::postgres::get_container_id(&repo_path);
    cleanup.add(gfs.clone());

    // Requested schema is mirrored.
    assert_eq!(
        psql(&gfs, "postgres", "SELECT relkind FROM pg_class WHERE relname='account' AND relnamespace='sales'::regnamespace"),
        "v"
    );
    assert_eq!(psql(&gfs, "postgres", "SELECT count(*) FROM sales.account"), "2");

    // The unrequested schema was never created locally.
    assert_eq!(
        psql(&gfs, "postgres", "SELECT count(*) FROM pg_namespace WHERE nspname='hr'"),
        "0",
        "unrequested schema must not be mirrored"
    );
    assert_eq!(
        psql(&gfs, "postgres", "SELECT count(*) FROM gfs_sync.table_meta WHERE schema_name='hr'"),
        "0"
    );
}

/// A table using a user-defined ENUM type clones with full fidelity: the enum
/// type is mirrored locally, the table becomes an overlay, and reads/writes of
/// enum values work through it.
#[test]
#[serial]
fn clone_supports_enum_types() {
    let repo = TempDir::new().unwrap();
    let repo_path = repo.path().to_path_buf();
    let mut cleanup = Cleanup::new(repo);

    let remote = "gfs-e2e-enum-remote";
    cleanup.add(remote);
    let port = start_remote(remote, "postgres:16");
    seed_remote(remote, &format!(
        "{READER} CREATE TYPE mood AS ENUM ('sad','ok','happy'); \
         CREATE TABLE feelings (id bigint PRIMARY KEY, m mood NOT NULL); \
         INSERT INTO feelings VALUES (1,'happy'),(2,'sad'),(3,'ok'); {}", grant_reader()));

    let url = format!("postgres://gfs_reader:readerpw@host.docker.internal:{port}/shop");
    let out = run_clone(&url, &repo_path, Some("16"));
    assert!(out.status.success(), "clone failed: {}", String::from_utf8_lossy(&out.stderr));

    let gfs = common::postgres::get_container_id(&repo_path);
    cleanup.add(gfs.clone());

    // The enum type was mirrored and the table cloned as an overlay (not skipped).
    assert_eq!(
        psql(&gfs, "postgres", "SELECT count(*) FROM pg_type WHERE typname='mood' AND typtype='e'"),
        "1",
        "the ENUM type must be mirrored locally"
    );
    assert_eq!(
        psql(&gfs, "postgres", "SELECT relkind FROM pg_class WHERE relname='feelings' AND relnamespace='public'::regnamespace"),
        "v",
        "the enum-typed table must clone as an overlay view (not be skipped)"
    );
    assert_eq!(psql(&gfs, "postgres", "SELECT count(*) FROM feelings"), "3");
    assert_eq!(psql(&gfs, "postgres", "SELECT m FROM feelings WHERE id=1"), "happy");

    // Write an enum value through the overlay; label ordering is preserved.
    psql(&gfs, "postgres", "INSERT INTO feelings VALUES (4,'ok')");
    assert_eq!(psql(&gfs, "postgres", "SELECT count(*) FROM feelings"), "4");
    psql(&gfs, "postgres", "UPDATE feelings SET m='happy' WHERE id=2");
    assert_eq!(psql(&gfs, "postgres", "SELECT m FROM feelings WHERE id=2"), "happy");
    // State is now {1:happy, 2:happy, 3:ok, 4:ok}; only 'happy' > 'ok', which
    // proves the enum's ordering (sad<ok<happy) is honored locally.
    assert_eq!(
        psql(&gfs, "postgres", "SELECT count(*) FROM feelings WHERE m > 'ok'"),
        "2",
        "enum ordering (sad<ok<happy) must work locally"
    );

    // Remote untouched.
    assert_eq!(psql(remote, "shop", "SELECT count(*) FROM feelings"), "3");
}

/// A table using a DOMAIN type clones fully: the domain (base type + CHECK) is
/// mirrored locally, so the table becomes an overlay and the domain's CHECK is
/// enforced on local writes.
#[test]
#[serial]
fn clone_supports_domain_types() {
    let repo = TempDir::new().unwrap();
    let repo_path = repo.path().to_path_buf();
    let mut cleanup = Cleanup::new(repo);

    let remote = "gfs-e2e-domain-remote";
    cleanup.add(remote);
    let port = start_remote(remote, "postgres:16");
    seed_remote(remote, &format!(
        "{READER} CREATE DOMAIN us_zip AS text CHECK (VALUE ~ '^[0-9]{{5}}$'); \
         CREATE TABLE addrs (id bigint PRIMARY KEY, zip us_zip NOT NULL); \
         INSERT INTO addrs VALUES (1,'94105'),(2,'10001'); {}", grant_reader()));

    let url = format!("postgres://gfs_reader:readerpw@host.docker.internal:{port}/shop");
    let out = run_clone(&url, &repo_path, Some("16"));
    assert!(out.status.success(), "clone failed: {}", String::from_utf8_lossy(&out.stderr));

    let gfs = common::postgres::get_container_id(&repo_path);
    cleanup.add(gfs.clone());

    // Domain mirrored, table cloned as an overlay.
    assert_eq!(
        psql(&gfs, "postgres", "SELECT count(*) FROM pg_type WHERE typname='us_zip' AND typtype='d'"),
        "1",
        "the DOMAIN type must be mirrored locally"
    );
    assert_eq!(
        psql(&gfs, "postgres", "SELECT relkind FROM pg_class WHERE relname='addrs' AND relnamespace='public'::regnamespace"),
        "v"
    );
    assert_eq!(psql(&gfs, "postgres", "SELECT count(*) FROM addrs"), "2");
    assert_eq!(psql(&gfs, "postgres", "SELECT zip FROM addrs WHERE id=1"), "94105");

    // A valid value writes; an invalid one is rejected by the domain CHECK.
    psql(&gfs, "postgres", "INSERT INTO addrs VALUES (3,'60601')");
    assert_eq!(psql(&gfs, "postgres", "SELECT count(*) FROM addrs"), "3");
    let bad = psql_try(&gfs, "postgres", "INSERT INTO addrs VALUES (4,'nope')");
    assert!(!bad.status.success(), "domain CHECK must reject an invalid value locally");

    assert_eq!(psql(remote, "shop", "SELECT count(*) FROM addrs"), "2");
}

/// A table using a COMPOSITE type (whose attribute is itself a mirrored ENUM)
/// clones fully — exercising type-mirroring order (enum before composite).
#[test]
#[serial]
fn clone_supports_composite_types() {
    let repo = TempDir::new().unwrap();
    let repo_path = repo.path().to_path_buf();
    let mut cleanup = Cleanup::new(repo);

    let remote = "gfs-e2e-composite-type-remote";
    cleanup.add(remote);
    let port = start_remote(remote, "postgres:16");
    seed_remote(remote, &format!(
        "{READER} CREATE TYPE mood AS ENUM ('sad','ok','happy'); \
         CREATE TYPE person AS (name text, feeling mood); \
         CREATE TABLE people (id bigint PRIMARY KEY, p person NOT NULL); \
         INSERT INTO people VALUES (1, ROW('Ada','happy')), (2, ROW('Alan','ok')); {}",
        grant_reader()));

    let url = format!("postgres://gfs_reader:readerpw@host.docker.internal:{port}/shop");
    let out = run_clone(&url, &repo_path, Some("16"));
    assert!(out.status.success(), "clone failed: {}", String::from_utf8_lossy(&out.stderr));

    let gfs = common::postgres::get_container_id(&repo_path);
    cleanup.add(gfs.clone());

    // Composite type mirrored (with its 2 attributes), table cloned as overlay.
    assert_eq!(
        psql(&gfs, "postgres", "SELECT count(*) FROM pg_type WHERE typname='person' AND typtype='c'"),
        "1",
        "the COMPOSITE type must be mirrored locally"
    );
    assert_eq!(
        psql(&gfs, "postgres",
            "SELECT count(*) FROM pg_attribute WHERE attrelid=(SELECT typrelid FROM pg_type WHERE typname='person') AND attnum>0 AND NOT attisdropped"),
        "2"
    );
    assert_eq!(
        psql(&gfs, "postgres", "SELECT relkind FROM pg_class WHERE relname='people' AND relnamespace='public'::regnamespace"),
        "v"
    );
    assert_eq!(psql(&gfs, "postgres", "SELECT count(*) FROM people"), "2");
    assert_eq!(psql(&gfs, "postgres", "SELECT (p).name FROM people WHERE id=1"), "Ada");
    assert_eq!(psql(&gfs, "postgres", "SELECT (p).feeling FROM people WHERE id=1"), "happy");

    // Write a composite (with enum attribute) through the overlay.
    psql(&gfs, "postgres", "INSERT INTO people VALUES (3, ROW('Grace','happy')::person)");
    assert_eq!(psql(&gfs, "postgres", "SELECT count(*) FROM people"), "3");
    assert_eq!(psql(&gfs, "postgres", "SELECT (p).name FROM people WHERE id=3"), "Grace");

    assert_eq!(psql(remote, "shop", "SELECT count(*) FROM people"), "2");
}

/// Cloning a remote with no user tables succeeds: the FDW link and `gfs_sync`
/// infrastructure are set up, but there are no overlays and the catalog is empty.
#[test]
#[serial]
fn clone_into_empty_database() {
    let repo = TempDir::new().unwrap();
    let repo_path = repo.path().to_path_buf();
    let mut cleanup = Cleanup::new(repo);

    let remote = "gfs-e2e-empty-remote";
    cleanup.add(remote);
    let port = start_remote(remote, "postgres:16");
    // Role + grants only — no tables at all.
    seed_remote(remote, &format!("{READER} {}", grant_reader()));

    let url = format!("postgres://gfs_reader:readerpw@host.docker.internal:{port}/shop");
    let out = run_clone(&url, &repo_path, Some("16"));
    assert!(
        out.status.success(),
        "clone of an empty database must succeed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    let gfs = common::postgres::get_container_id(&repo_path);
    cleanup.add(gfs.clone());

    // Bootstrap ran (FDW server + sync schema exist) but nothing was mirrored.
    assert_eq!(
        psql(&gfs, "postgres", "SELECT count(*) FROM pg_foreign_server WHERE srvname='gfs_remote_srv'"),
        "1"
    );
    assert_eq!(
        psql(&gfs, "postgres", "SELECT count(*) FROM pg_namespace WHERE nspname='gfs_sync'"),
        "1"
    );
    assert_eq!(psql(&gfs, "postgres", "SELECT count(*) FROM gfs_sync.table_meta"), "0");
}

/// `gfs clone` is one-shot: re-running it into a directory that is already a GFS
/// repository fails cleanly (no overwrite), and the existing clone stays usable.
#[test]
#[serial]
fn clone_into_existing_repo_fails() {
    let repo = TempDir::new().unwrap();
    let repo_path = repo.path().to_path_buf();
    let mut cleanup = Cleanup::new(repo);

    let remote = "gfs-e2e-reclone-remote";
    cleanup.add(remote);
    let port = start_remote(remote, "postgres:16");
    seed_remote(remote, &format!(
        "{READER} CREATE TABLE orders (id bigint PRIMARY KEY, name text NOT NULL); \
         INSERT INTO orders SELECT g,'n'||g FROM generate_series(1,10) g; {}", grant_reader()));

    let url = format!("postgres://gfs_reader:readerpw@host.docker.internal:{port}/shop");

    // First clone succeeds.
    let first = run_clone(&url, &repo_path, Some("16"));
    assert!(first.status.success(), "first clone failed: {}", String::from_utf8_lossy(&first.stderr));
    let gfs = common::postgres::get_container_id(&repo_path);
    cleanup.add(gfs.clone());
    assert_eq!(psql(&gfs, "postgres", "SELECT count(*) FROM orders"), "10");

    // Second clone into the same repo must fail (already initialized).
    let second = run_clone(&url, &repo_path, Some("16"));
    assert!(
        !second.status.success(),
        "re-cloning into an existing repo should fail, but it succeeded:\nstdout: {}",
        String::from_utf8_lossy(&second.stdout)
    );

    // The original clone is untouched and still serves its data.
    assert_eq!(
        psql(&gfs, "postgres", "SELECT count(*) FROM orders"),
        "10",
        "the existing clone must survive a rejected re-clone"
    );
}

/// Challenge: a STORED generated column must not break writes, and must be
/// computed for locally-written rows.
#[test]
#[serial]
fn clone_handles_generated_columns() {
    let repo = TempDir::new().unwrap();
    let repo_path = repo.path().to_path_buf();
    let mut cleanup = Cleanup::new(repo);

    let remote = "gfs-e2e-gen-remote";
    cleanup.add(remote);
    let port = start_remote(remote, "postgres:17");
    seed_remote(remote, &format!(
        "{READER} CREATE TABLE docs (id bigint PRIMARY KEY, body text NOT NULL, \
           body_tsv tsvector GENERATED ALWAYS AS (to_tsvector('english', body)) STORED); \
         INSERT INTO docs (id, body) VALUES (1,'hello world'),(2,'lazy clone'); {}", grant_reader()));

    let url = format!("postgres://gfs_reader:readerpw@host.docker.internal:{port}/shop");
    let out = run_clone(&url, &repo_path, Some("17"));
    assert!(out.status.success(), "clone failed: {}", String::from_utf8_lossy(&out.stderr));
    let gfs = common::postgres::get_container_id(&repo_path);
    cleanup.add(gfs.clone());

    assert_eq!(psql(&gfs, "postgres", "SELECT count(*) FROM docs"), "2");
    // Remote rows: generated column is read live and populated.
    assert_eq!(psql(&gfs, "postgres", "SELECT body_tsv IS NOT NULL FROM docs WHERE id=1"), "t");

    // Write a new row WITHOUT the generated column (the only legal way).
    let ins = psql_try(&gfs, "postgres", "INSERT INTO docs (id, body) VALUES (3, 'fresh doc')");
    assert!(
        ins.status.success(),
        "insert omitting a generated column must succeed:\n{}",
        String::from_utf8_lossy(&ins.stderr)
    );
    assert_eq!(psql(&gfs, "postgres", "SELECT body FROM docs WHERE id=3"), "fresh doc");
    // The generated column must be computed for the local row.
    assert_eq!(
        psql(&gfs, "postgres", "SELECT body_tsv IS NOT NULL FROM docs WHERE id=3"),
        "t",
        "generated column should be computed for a locally-written row"
    );
}

/// Challenge: reserved-word / mixed-case / quoted identifiers must round-trip
/// through the overlay (views, triggers, sequences are all %I-quoted).
#[test]
#[serial]
fn clone_handles_reserved_and_mixedcase_identifiers() {
    let repo = TempDir::new().unwrap();
    let repo_path = repo.path().to_path_buf();
    let mut cleanup = Cleanup::new(repo);

    let remote = "gfs-e2e-ident-remote";
    cleanup.add(remote);
    let port = start_remote(remote, "postgres:17");
    seed_remote(remote, &format!(
        "{READER} CREATE TABLE \"Order\" (\"Id\" bigint PRIMARY KEY, \"select\" text NOT NULL, \"UserName\" text); \
         INSERT INTO \"Order\" (\"Id\",\"select\",\"UserName\") VALUES (1,'a','bob'); {}", grant_reader()));

    let url = format!("postgres://gfs_reader:readerpw@host.docker.internal:{port}/shop");
    let out = run_clone(&url, &repo_path, Some("17"));
    assert!(out.status.success(), "clone failed: {}", String::from_utf8_lossy(&out.stderr));
    let gfs = common::postgres::get_container_id(&repo_path);
    cleanup.add(gfs.clone());

    assert_eq!(psql(&gfs, "postgres", "SELECT count(*) FROM \"Order\""), "1");
    psql(&gfs, "postgres", "INSERT INTO \"Order\" (\"Id\",\"select\",\"UserName\") VALUES (2,'b','alice')");
    assert_eq!(psql(&gfs, "postgres", "SELECT count(*) FROM \"Order\""), "2");
    psql(&gfs, "postgres", "UPDATE \"Order\" SET \"select\"='x' WHERE \"Id\"=1");
    assert_eq!(psql(&gfs, "postgres", "SELECT \"select\" FROM \"Order\" WHERE \"Id\"=1"), "x");
    assert_eq!(psql(&gfs, "postgres", "SELECT count(*) FROM \"Order\""), "2");
}

/// Challenge: a table keyed only by a UNIQUE index on a NULLABLE column.
#[test]
#[serial]
fn clone_handles_nullable_unique_key() {
    let repo = TempDir::new().unwrap();
    let repo_path = repo.path().to_path_buf();
    let mut cleanup = Cleanup::new(repo);

    let remote = "gfs-e2e-nullkey-remote";
    cleanup.add(remote);
    let port = start_remote(remote, "postgres:17");
    seed_remote(remote, &format!(
        "{READER} CREATE TABLE widgets (sku text, qty int NOT NULL); \
         CREATE UNIQUE INDEX widgets_sku_uq ON widgets (sku); \
         INSERT INTO widgets VALUES ('a',1),('b',2),(NULL,3); {}", grant_reader()));

    let url = format!("postgres://gfs_reader:readerpw@host.docker.internal:{port}/shop");
    let out = run_clone(&url, &repo_path, Some("17"));
    assert!(out.status.success(), "clone failed: {}", String::from_utf8_lossy(&out.stderr));
    let gfs = common::postgres::get_container_id(&repo_path);
    cleanup.add(gfs.clone());

    // All rows readable (including the NULL-sku one) with no duplication.
    assert_eq!(psql(&gfs, "postgres", "SELECT count(*) FROM widgets"), "3");
    assert_eq!(psql(&gfs, "postgres", "SELECT count(*) FROM widgets WHERE sku IS NULL"), "1");
    // A write to a non-null-key row diverges correctly.
    psql(&gfs, "postgres", "UPDATE widgets SET qty=99 WHERE sku='a'");
    assert_eq!(psql(&gfs, "postgres", "SELECT qty FROM widgets WHERE sku='a'"), "99");
    assert_eq!(psql(&gfs, "postgres", "SELECT count(*) FROM widgets"), "3");
}

/// Network elision: after `gfs_sync.warm_range` hydrates a key range, a read
/// whose key falls in that range is served locally and the foreign scan is
/// **pruned** (no remote contact), while reads outside the range still hit the
/// remote — and unconstrained scans stay correct.
#[test]
#[serial]
fn clone_warm_range_elides_remote_reads() {
    let repo = TempDir::new().unwrap();
    let repo_path = repo.path().to_path_buf();
    let mut cleanup = Cleanup::new(repo);

    let remote = "gfs-e2e-elision-remote";
    cleanup.add(remote);
    let port = start_remote(remote, "postgres:16");
    seed_remote(remote, &format!(
        "{READER} CREATE TABLE orders (id bigint PRIMARY KEY, name text NOT NULL); \
         INSERT INTO orders SELECT g,'n'||g FROM generate_series(1,10000) g; {}", grant_reader()));

    let url = format!("postgres://gfs_reader:readerpw@host.docker.internal:{port}/shop");
    let out = run_clone(&url, &repo_path, Some("16"));
    assert!(out.status.success(), "clone failed: {}", String::from_utf8_lossy(&out.stderr));
    let gfs = common::postgres::get_container_id(&repo_path);
    cleanup.add(gfs.clone());

    // Warm ids [1,1000] into the local store (records the range + rebuilds the
    // foreign table's exclusion CHECK).
    psql(&gfs, "postgres", "SELECT gfs_sync.warm_range('public','orders','1','1000')");

    // A cached-range read: the planner prunes the foreign branch entirely.
    let plan_cached = psql(&gfs, "postgres", "EXPLAIN (VERBOSE) SELECT * FROM orders WHERE id = 42");
    assert!(
        !plan_cached.contains("Foreign Scan"),
        "cached-range read must not touch the remote, plan was:\n{plan_cached}"
    );
    // A non-cached read still federates to the remote.
    let plan_remote = psql(&gfs, "postgres", "EXPLAIN (VERBOSE) SELECT * FROM orders WHERE id = 5000");
    assert!(
        plan_remote.contains("Foreign Scan"),
        "non-cached read should still hit the remote, plan was:\n{plan_remote}"
    );

    // Correctness in all cases.
    assert_eq!(psql(&gfs, "postgres", "SELECT name FROM orders WHERE id = 42"), "n42");
    assert_eq!(psql(&gfs, "postgres", "SELECT name FROM orders WHERE id = 5000"), "n5000");
    assert_eq!(
        psql(&gfs, "postgres", "SELECT count(*) FROM orders"),
        "10000",
        "unconstrained scan stays correct (anti-join dedups despite the exclusion CHECK)"
    );
}

/// Query-driven warming: `gfs_sync.warm_query_chunks(sql)` (what a proxy/cron
/// calls) expands the query's key span to chunk boundaries and warms the whole
/// chunk — so the queried key AND its neighbours in the chunk are then elided,
/// while a key in another chunk still federates to the remote.
#[test]
#[serial]
fn clone_warm_query_chunks_drives_elision() {
    let repo = TempDir::new().unwrap();
    let repo_path = repo.path().to_path_buf();
    let mut cleanup = Cleanup::new(repo);

    let remote = "gfs-e2e-chunk-remote";
    cleanup.add(remote);
    let port = start_remote(remote, "postgres:16");
    seed_remote(remote, &format!(
        "{READER} CREATE TABLE orders (id bigint PRIMARY KEY, name text NOT NULL); \
         INSERT INTO orders SELECT g,'n'||g FROM generate_series(1,10000) g; {}", grant_reader()));

    let url = format!("postgres://gfs_reader:readerpw@host.docker.internal:{port}/shop");
    let out = run_clone(&url, &repo_path, Some("16"));
    assert!(out.status.success(), "clone failed: {}", String::from_utf8_lossy(&out.stderr));
    let gfs = common::postgres::get_container_id(&repo_path);
    cleanup.add(gfs.clone());

    // Drive warming from a single point-read SQL, chunk size 1000 → warms the
    // chunk [4000,4999] that contains id=4242.
    psql(&gfs, "postgres",
        "SELECT gfs_sync.warm_query_chunks('SELECT * FROM orders WHERE id = 4242', 1000)");

    // The queried key is elided...
    let p_hit = psql(&gfs, "postgres", "EXPLAIN (VERBOSE) SELECT * FROM orders WHERE id = 4242");
    assert!(!p_hit.contains("Foreign Scan"), "warmed key must be elided:\n{p_hit}");
    // ...and so is a NEIGHBOUR in the same chunk that was never queried.
    let p_neighbour = psql(&gfs, "postgres", "EXPLAIN (VERBOSE) SELECT * FROM orders WHERE id = 4900");
    assert!(!p_neighbour.contains("Foreign Scan"), "chunk neighbour must be elided:\n{p_neighbour}");
    // A key in a different chunk still federates.
    let p_other = psql(&gfs, "postgres", "EXPLAIN (VERBOSE) SELECT * FROM orders WHERE id = 8000");
    assert!(p_other.contains("Foreign Scan"), "key outside the warmed chunk should hit the remote:\n{p_other}");

    // Correctness across all of them.
    assert_eq!(psql(&gfs, "postgres", "SELECT name FROM orders WHERE id = 4900"), "n4900");
    assert_eq!(psql(&gfs, "postgres", "SELECT name FROM orders WHERE id = 8000"), "n8000");
    assert_eq!(psql(&gfs, "postgres", "SELECT count(*) FROM orders"), "10000");
}

/// Intersection of the two concurrent features: network elision (`warm_range`)
/// hydrating a table that has a STORED generated column. The local store is
/// created `INCLUDING GENERATED`, so hydration cannot `INSERT ... SELECT *`
/// (Postgres refuses a non-DEFAULT value into a generated column); it must list
/// the non-generated columns explicitly, like the overlay's write trigger does.
#[test]
#[serial]
fn clone_warm_range_handles_generated_columns() {
    let repo = TempDir::new().unwrap();
    let repo_path = repo.path().to_path_buf();
    let mut cleanup = Cleanup::new(repo);

    let remote = "gfs-e2e-warmgen-remote";
    cleanup.add(remote);
    let port = start_remote(remote, "postgres:16");
    seed_remote(remote, &format!(
        "{READER} CREATE TABLE orders (id bigint PRIMARY KEY, qty int NOT NULL, \
         total int GENERATED ALWAYS AS (qty * 10) STORED); \
         INSERT INTO orders(id, qty) SELECT g, g FROM generate_series(1,2000) g; {}", grant_reader()));

    let url = format!("postgres://gfs_reader:readerpw@host.docker.internal:{port}/shop");
    let out = run_clone(&url, &repo_path, Some("16"));
    assert!(out.status.success(), "clone failed: {}", String::from_utf8_lossy(&out.stderr));
    let gfs = common::postgres::get_container_id(&repo_path);
    cleanup.add(gfs.clone());

    // Hydration must not choke on the generated column.
    let warm = psql_try(&gfs, "postgres",
        "SELECT gfs_sync.warm_range('public','orders','1','1000')");
    assert!(
        warm.status.success(),
        "warm_range must tolerate generated columns, got:\n{}",
        String::from_utf8_lossy(&warm.stderr)
    );

    // The warmed local rows recomputed the generated column locally...
    assert_eq!(psql(&gfs, "postgres", "SELECT total FROM orders WHERE id = 42"), "420");
    // ...and the warmed range is elided.
    let plan = psql(&gfs, "postgres", "EXPLAIN (VERBOSE) SELECT * FROM orders WHERE id = 42");
    assert!(!plan.contains("Foreign Scan"), "cached-range read must be elided:\n{plan}");
    // A non-warmed key still federates and stays correct.
    assert_eq!(psql(&gfs, "postgres", "SELECT total FROM orders WHERE id = 1500"), "15000");
    assert_eq!(psql(&gfs, "postgres", "SELECT count(*) FROM orders"), "2000");
}
