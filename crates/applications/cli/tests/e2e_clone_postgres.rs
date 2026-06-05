//! End-to-end test for `gfs clone` (RFC 008 lazy clone) against a real remote Postgres.
//!
//! Flow:
//!   1. Start a throwaway "remote" Postgres (published port) and seed it read-only.
//!   2. Run the real `gfs clone` binary on the gfs-postgres image: it provisions a
//!      local GFS Postgres and bootstraps the planner-hook clone (foreign tables +
//!      real local tables registered in gfs.clone_source).
//!   3. Validate, against the cloned database (what an app sees on a direct
//!      connection): reads are correct, writes diverge locally, the remote is
//!      untouched.
//!
//! macOS-only, consistent with the other e2e suites (APFS storage backend), and
//! relies on Docker Desktop's `host.docker.internal` so the GFS container reaches
//! the remote's published port. Docker or Podman must be running.

#![cfg(target_os = "macos")]

mod common;

use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

use common::container_runtime::runtime_command;
use tempfile::TempDir;

const REMOTE_NAME: &str = "gfs-e2e-clone-remote";

/// Cleans up the remote container, the GFS-provisioned container, and the repo.
struct Cleanup {
    gfs_container: Option<String>,
    repo: Option<TempDir>,
}

impl Drop for Cleanup {
    fn drop(&mut self) {
        if let Some(c) = &self.gfs_container {
            let _ = runtime_command().args(["rm", "-f", c]).output();
        }
        let _ = runtime_command().args(["rm", "-f", REMOTE_NAME]).output();
        drop(self.repo.take());
    }
}

fn psql_remote(query: &str) -> String {
    let out = runtime_command()
        .args([
            "exec", REMOTE_NAME, "psql", "-U", "postgres", "-d", "shop", "-tAc", query,
        ])
        .output()
        .expect("psql on remote");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// Run psql inside the cloned GFS container (DB `postgres`).
fn psql_gfs(container: &str, query: &str) -> String {
    common::postgres::run_psql_select(container, query)
        .trim()
        .to_string()
}

#[test]
fn e2e_clone_postgres() {
    // The planner-hook clone needs the gfs extension image; skip (don't fail) if absent.
    let img_ok = runtime_command()
        .args(["image", "inspect", "gfs-postgres:16"])
        .output().map(|o| o.status.success()).unwrap_or(false);
    if !img_ok {
        eprintln!("SKIP: image gfs-postgres:16 absent — build: docker build -t gfs-postgres:16 crates/extensions/gfs");
        return;
    }
    let repo = TempDir::new().expect("temp repo");
    let repo_path = repo.path().to_path_buf();
    let mut cleanup = Cleanup {
        gfs_container: None,
        repo: Some(repo),
    };

    // 1. Start the remote with a Docker-assigned host port, then read it back.
    let _ = runtime_command().args(["rm", "-f", REMOTE_NAME]).output();
    let started = runtime_command()
        .args([
            "run", "-d", "--name", REMOTE_NAME,
            "-e", "POSTGRES_PASSWORD=postgres",
            "-e", "POSTGRES_DB=shop",
            "-p", "127.0.0.1::5432",
            "postgres:16",
        ])
        .output()
        .expect("start remote container");
    assert!(
        started.status.success(),
        "failed to start remote: {}",
        String::from_utf8_lossy(&started.stderr)
    );

    let port_out = runtime_command()
        .args(["port", REMOTE_NAME, "5432"])
        .output()
        .expect("docker port");
    let mapped = String::from_utf8_lossy(&port_out.stdout);
    let host_port = mapped
        .lines()
        .next()
        .and_then(|l| l.rsplit(':').next())
        .map(|s| s.trim().to_string())
        .expect("parse mapped port");

    // 2. Wait for readiness, then seed (read-only role + a table with a bigint PK).
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let ready = runtime_command()
            .args(["exec", REMOTE_NAME, "pg_isready", "-U", "postgres", "-d", "shop"])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if ready {
            break;
        }
        assert!(Instant::now() < deadline, "remote never became ready");
        thread::sleep(Duration::from_millis(500));
    }

    let seed = "DO $$ BEGIN IF NOT EXISTS (SELECT FROM pg_roles WHERE rolname='gfs_reader') \
                THEN CREATE ROLE gfs_reader LOGIN PASSWORD 'readerpw'; END IF; END $$; \
                CREATE TABLE orders (id bigint PRIMARY KEY, customer text NOT NULL, amount numeric(10,2) NOT NULL); \
                INSERT INTO orders SELECT g, 'cust_'||(g%100), (g%500)+0.5 FROM generate_series(1,1000) g; \
                GRANT USAGE ON SCHEMA public TO gfs_reader; \
                GRANT SELECT ON ALL TABLES IN SCHEMA public TO gfs_reader;";
    let seeded = runtime_command()
        .args(["exec", REMOTE_NAME, "psql", "-U", "postgres", "-d", "shop", "-v", "ON_ERROR_STOP=1", "-c", seed])
        .output()
        .expect("seed remote");
    assert!(
        seeded.status.success(),
        "seed failed: {}",
        String::from_utf8_lossy(&seeded.stderr)
    );
    assert_eq!(psql_remote("SELECT count(*) FROM orders"), "1000");

    // 3. Run the real `gfs clone` binary.
    let url = format!("postgres://gfs_reader:readerpw@host.docker.internal:{host_port}/shop");
    let out = Command::new(env!("CARGO_BIN_EXE_gfs"))
        .args([
            "clone",
            "--from",
            &url,
            repo_path.to_str().unwrap(),
            "--image",
            "gfs-postgres:16",
            "--database-version",
            "16",
        ])
        .output()
        .expect("run gfs clone");
    assert!(
        out.status.success(),
        "gfs clone failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    let gfs_container = common::postgres::get_container_id(&repo_path);
    cleanup.gfs_container = Some(gfs_container.clone());

    // 4. Validate the planner-hook clone on the cloned DB (what a direct app
    // connection sees). public.orders is a REAL local table ('r') registered in
    // gfs.clone_source; there is no overlay view and no gfs_sync schema. Reads are
    // copy-on-read and return the source's data.
    assert_eq!(
        psql_gfs(&gfs_container, "SELECT relkind FROM pg_class WHERE relname='orders' AND relnamespace='public'::regnamespace"),
        "r",
        "public.orders should be the real local table"
    );
    assert_eq!(
        psql_gfs(&gfs_container, "SELECT count(*) FROM pg_namespace WHERE nspname LIKE 'gfs_ovl__%'"),
        "0",
        "the removed overlay-view schema must not exist"
    );
    assert_eq!(
        psql_gfs(&gfs_container, "SELECT count(*) FROM gfs.clone_source WHERE relid::text='orders'"),
        "1",
        "orders should be registered in gfs.clone_source"
    );
    assert_eq!(
        psql_gfs(&gfs_container, "SELECT count(*) FROM orders"),
        "1000",
        "copy-on-read should match the remote"
    );
    assert_eq!(
        psql_gfs(&gfs_container, "SELECT customer FROM orders WHERE id=42"),
        "cust_42"
    );

    // Local write → diverges locally, remote stays untouched (write-safety guard).
    psql_gfs(&gfs_container, "UPDATE orders SET customer='LOCAL' WHERE id=42");
    assert_eq!(
        psql_gfs(&gfs_container, "SELECT customer FROM orders WHERE id=42"),
        "LOCAL",
        "local update should be visible on the clone"
    );
    psql_gfs(&gfs_container, "INSERT INTO orders (id,customer,amount) VALUES (99999,'NEW',1.0)");
    assert_eq!(psql_gfs(&gfs_container, "SELECT count(*) FROM orders"), "1001");

    // The remote must be untouched by the local writes.
    assert_eq!(psql_remote("SELECT customer FROM orders WHERE id=42"), "cust_42");
    assert_eq!(psql_remote("SELECT count(*) FROM orders"), "1000");

    // cleanup runs on drop (gfs container + remote + repo)
    drop(cleanup);
}
