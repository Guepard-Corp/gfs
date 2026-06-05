//! End-to-end regression tests for `gfs clone` (RFC 008) on the current
//! **planner-hook** model (the former overlay/TAM is gone). Each test starts a
//! throwaway remote Postgres, runs the real `gfs clone` binary onto the
//! `gfs-postgres:16` image (which ships the `gfs` extension), and asserts the
//! router's invariants end to end:
//!   * clone tables are REAL tables registered in `gfs.clone_source` (no overlay
//!     view, no `gfs_sync`), FKs dropped, generated columns mirrored;
//!   * copy-on-read returns results equal to the source;
//!   * an integer/temporal key range hydrates then elides (re-ask hits no source);
//!   * a join is federated and returns the source's result;
//!   * local writes diverge on the clone while the SOURCE stays untouched (the
//!     write-safety guard: writes never federate to the source).
//!
//! macOS-only (consistent with the other e2e suites); Docker/Podman required;
//! relies on Docker Desktop's `host.docker.internal`. Tests SKIP (no failure) when
//! the `gfs-postgres:16` image is absent — build it with
//! `docker build -t gfs-postgres:16 crates/extensions/gfs`.

#![cfg(target_os = "macos")]

mod common;

use std::path::Path;
use std::process::{Command, Output};
use std::thread;
use std::time::{Duration, Instant};

use common::container_runtime::runtime_command;
use serial_test::serial;
use tempfile::TempDir;

const GFS_IMAGE: &str = "gfs-postgres:16";

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

/// Start a remote postgres from `image` with a Docker-assigned host port and wait
/// for readiness. Returns the mapped host port.
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

fn seed_remote(name: &str, sql: &str) {
    let out = runtime_command()
        .args(["exec", name, "psql", "-U", "postgres", "-d", "shop", "-v", "ON_ERROR_STOP=1", "-c", sql])
        .output().expect("seed");
    assert!(out.status.success(), "seed failed: {}", String::from_utf8_lossy(&out.stderr));
}

const READER: &str =
    "DO $$ BEGIN IF NOT EXISTS (SELECT FROM pg_roles WHERE rolname='gfs_reader') \
     THEN CREATE ROLE gfs_reader LOGIN PASSWORD 'readerpw'; END IF; END $$; ";

/// Multi-table schema exercising: an integer range key (orders.id), a FK
/// (orders.customer_id -> customers.id), a STORED generated column
/// (orders.total_cents), and a temporal column (orders.placed_at).
const SCHEMA: &str = "\
    CREATE TABLE customers (id bigint PRIMARY KEY, name text NOT NULL); \
    INSERT INTO customers SELECT g, 'cust_'||g FROM generate_series(1,500) g; \
    CREATE TABLE orders ( \
        id bigint PRIMARY KEY, \
        customer_id bigint NOT NULL REFERENCES customers(id), \
        qty int NOT NULL, unit_cents int NOT NULL, \
        total_cents int GENERATED ALWAYS AS (qty*unit_cents) STORED, \
        placed_at date NOT NULL); \
    INSERT INTO orders (id, customer_id, qty, unit_cents, placed_at) \
        SELECT g, 1+(g%500), 1+(g%5), 100+(g%50), date '2024-01-01' + (g%364) \
        FROM generate_series(1,2000) g; \
    GRANT USAGE ON SCHEMA public TO gfs_reader; \
    GRANT SELECT ON ALL TABLES IN SCHEMA public TO gfs_reader;";

/// True if the gfs extension image is present; otherwise print a SKIP note.
fn gfs_image_present() -> bool {
    let ok = runtime_command()
        .args(["image", "inspect", GFS_IMAGE])
        .output().map(|o| o.status.success()).unwrap_or(false);
    if !ok {
        eprintln!("SKIP: image {GFS_IMAGE} absent — build: docker build -t {GFS_IMAGE} crates/extensions/gfs");
    }
    ok
}

fn run_clone(url: &str, repo: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_gfs"))
        .args([
            "clone", "--from", url, repo.to_str().unwrap(),
            "--image", GFS_IMAGE, "--database-version", "16",
        ])
        .output()
        .expect("run gfs clone")
}

/// Pin cost weights so the small seed tables stay NOT whole-ownable (horizon=0,
/// negligible=1): reads federate, key/time ranges hydrate-then-elide, writes
/// federate-classify -> the lazy paths are exercised deterministically.
fn pin_weights(clone: &str) {
    psql(clone, "postgres",
        "UPDATE gfs.cost SET net=1, source=20, negligible=1, horizon=0, ceiling=1000000000");
}

fn sum_fetched(clone: &str) -> i64 {
    psql(clone, "postgres", "SELECT COALESCE(sum(rows_fetched),0) FROM gfs.clones")
        .parse().unwrap_or(-1)
}

/// Start remote + seed + clone; returns (remote_name, clone_container, host_port).
/// Registers both containers with `cl` for cleanup.
fn setup(cl: &mut Cleanup, remote: &str, repo: &Path) -> String {
    let _ = remote;
    let port = start_remote(remote, "postgres:16");
    cl.add(remote.to_string());
    seed_remote(remote, &format!("{READER}{SCHEMA}"));
    let url = format!("postgres://gfs_reader:readerpw@host.docker.internal:{port}/shop");
    let out = run_clone(&url, repo);
    assert!(
        out.status.success(),
        "gfs clone failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    let clone = common::postgres::get_container_id(repo);
    cl.add(clone.clone());
    pin_weights(&clone);
    clone
}

/// Registration + copy-on-read correctness: real tables in gfs.clone_source (no
/// overlay/sync), FK dropped, generated column mirrored, reads equal the source.
#[test]
#[serial]
fn clone_registers_real_tables_and_reads_match_source() {
    if !gfs_image_present() { return; }
    let repo = TempDir::new().unwrap();
    let repo_path = repo.path().to_path_buf();
    let mut cl = Cleanup::new(repo);
    let remote = "gfs-e2e-rob-reg";
    let clone = setup(&mut cl, remote, &repo_path);

    assert_eq!(
        psql(&clone, "postgres", "SELECT relkind FROM pg_class WHERE relname='orders' AND relnamespace='public'::regnamespace"),
        "r", "public.orders is a real local table");
    assert_eq!(
        psql(&clone, "postgres", "SELECT count(*) FROM pg_namespace WHERE nspname LIKE 'gfs_ovl__%'"),
        "0", "no overlay-view schema (planner-hook model; gfs_sync holds the bootstrap funcs and stays)");
    assert_eq!(
        psql(&clone, "postgres", "SELECT count(*) FROM gfs.clone_source WHERE relid::text IN ('orders','customers')"),
        "2", "orders + customers registered as clones");
    assert_eq!(
        psql(&clone, "postgres", "SELECT count(*) FROM pg_constraint WHERE conrelid='orders'::regclass AND contype='f'"),
        "0", "FK dropped on the clone (child fetched before parent)");

    // Copy-on-read returns the source's data.
    assert_eq!(psql(&clone, "postgres", "SELECT count(*) FROM orders"), "2000");
    assert_eq!(
        psql(&clone, "postgres", "SELECT total_cents FROM orders WHERE id=10"),
        psql(remote, "shop", "SELECT total_cents FROM orders WHERE id=10"),
        "generated column mirrored correctly");
    drop(cl);
}

/// Integer range key hydrates the touched span then elides on re-ask; a temporal
/// (DATE) range key does the same after re-registering on `placed_at`.
#[test]
#[serial]
fn clone_range_and_temporal_hydration_elide() {
    if !gfs_image_present() { return; }
    let repo = TempDir::new().unwrap();
    let repo_path = repo.path().to_path_buf();
    let mut cl = Cleanup::new(repo);
    let remote = "gfs-e2e-rob-range";
    let clone = setup(&mut cl, remote, &repo_path);

    // P1: integer key range.
    let q = "SELECT id FROM orders WHERE id BETWEEN 100 AND 150 ORDER BY id";
    let f0 = sum_fetched(&clone);
    psql(&clone, "postgres", q);
    let f1 = sum_fetched(&clone);
    assert!(f1 > f0, "key range hydrated rows (f0={f0} f1={f1})");
    psql(&clone, "postgres", q);
    assert_eq!(sum_fetched(&clone), f1, "covered range elided — no new source fetch");
    assert_eq!(
        psql(&clone, "postgres", &format!("SELECT count(*) FROM ({q}) t")),
        psql(remote, "shop", &format!("SELECT count(*) FROM ({q}) t")),
        "range result matches source");

    // P5: temporal key range. Re-register orders on the DATE column.
    let sref = psql(&clone, "postgres", "SELECT source_ref FROM gfs.clone_source WHERE relid='orders'::regclass");
    psql(&clone, "postgres", "SELECT gfs.unregister_clone('orders'::regclass)");
    psql(&clone, "postgres", &format!("SELECT gfs.register_clone('orders'::regclass, '{sref}', 'placed_at')"));
    assert_eq!(
        psql(&clone, "postgres", "SELECT chunk_kind FROM gfs.clone_source WHERE relid='orders'::regclass"),
        "time", "orders re-registered on a temporal key");
    psql(&clone, "postgres", "TRUNCATE orders"); // fresh local state for the window

    let tq = "SELECT id FROM orders WHERE placed_at BETWEEN date '2024-02-01' AND date '2024-02-15' ORDER BY id";
    let t0 = sum_fetched(&clone);
    psql(&clone, "postgres", tq);
    let t1 = sum_fetched(&clone);
    assert!(t1 > t0, "temporal window hydrated (t0={t0} t1={t1})");
    psql(&clone, "postgres", tq);
    assert_eq!(sum_fetched(&clone), t1, "temporal window elided — no new source fetch");
    assert_eq!(
        psql(&clone, "postgres", &format!("SELECT count(*) FROM ({tq}) t")),
        psql(remote, "shop", &format!("SELECT count(*) FROM ({tq}) t")),
        "temporal result matches source");
    drop(cl);
}

/// A join is federated to the source and returns the source's result.
#[test]
#[serial]
fn clone_federates_join_matching_source() {
    if !gfs_image_present() { return; }
    let repo = TempDir::new().unwrap();
    let repo_path = repo.path().to_path_buf();
    let mut cl = Cleanup::new(repo);
    let remote = "gfs-e2e-rob-join";
    let clone = setup(&mut cl, remote, &repo_path);

    let jq = "SELECT c.name, count(*) c FROM orders o JOIN customers c ON c.id=o.customer_id \
              GROUP BY c.name ORDER BY c.name LIMIT 10";
    let ck = format!("SELECT md5(string_agg(t::text,'|' ORDER BY t::text)) FROM ({jq}) t");
    assert_eq!(
        psql(&clone, "postgres", &ck),
        psql(remote, "shop", &ck),
        "federated join result matches source");
    drop(cl);
}

/// Local writes diverge on the clone; the SOURCE stays untouched — including a
/// write whose scan federate-classifies (the write-safety guard must keep it
/// local instead of pushing it to the source).
#[test]
#[serial]
fn clone_local_writes_leave_source_untouched() {
    if !gfs_image_present() { return; }
    let repo = TempDir::new().unwrap();
    let repo_path = repo.path().to_path_buf();
    let mut cl = Cleanup::new(repo);
    let remote = "gfs-e2e-rob-write";
    let clone = setup(&mut cl, remote, &repo_path);

    // UPDATE with a subquery qual -> federate-classifies; the guard whole-hydrates
    // locally instead of writing the source.
    psql(&clone, "postgres", "UPDATE orders SET unit_cents=99999 WHERE id=(SELECT min(id) FROM orders)");
    assert_eq!(psql(&clone, "postgres", "SELECT unit_cents FROM orders WHERE id=1"), "99999", "local UPDATE applied");
    assert_ne!(psql(remote, "shop", "SELECT unit_cents FROM orders WHERE id=1"), "99999", "SOURCE not updated");
    assert_eq!(psql(&clone, "postgres", "SELECT federate_calls FROM gfs.clones WHERE clone='orders'"), "0",
        "the write did not federate (it whole-hydrated locally)");

    // INSERT diverges locally.
    psql(&clone, "postgres", "INSERT INTO orders (id,customer_id,qty,unit_cents,placed_at) VALUES (999999,1,1,1,date '2024-01-01')");
    assert_eq!(psql(&clone, "postgres", "SELECT count(*) FROM orders WHERE id=999999"), "1");
    assert_eq!(psql(remote, "shop", "SELECT count(*) FROM orders WHERE id=999999"), "0", "INSERT not on source");

    // DELETE (federate-classifying) leaves the source row count unchanged.
    let src_before = psql(remote, "shop", "SELECT count(*) FROM orders");
    psql(&clone, "postgres", "DELETE FROM orders WHERE id < (SELECT 50 FROM orders LIMIT 1)");
    assert_eq!(psql(remote, "shop", "SELECT count(*) FROM orders"), src_before, "SOURCE rows not deleted");
    drop(cl);
}

/// A locally-deleted row stays deleted: a later whole-table warm must NOT resurrect
/// it (copy-on-write tombstones).
#[test]
#[serial]
fn clone_local_delete_not_resurrected_by_warm() {
    if !gfs_image_present() { return; }
    let repo = TempDir::new().unwrap();
    let repo_path = repo.path().to_path_buf();
    let mut cl = Cleanup::new(repo);
    let remote = "gfs-e2e-rob-tomb";
    let clone = setup(&mut cl, remote, &repo_path);

    psql(&clone, "postgres", "DELETE FROM orders WHERE id=5");
    assert_eq!(psql(&clone, "postgres", "SELECT count(*) FROM orders WHERE id=5"), "0", "row deleted locally");
    assert_eq!(psql(&clone, "postgres", "SELECT count(*) FROM gfs.tombstone WHERE relid='orders'::regclass"), "1",
        "delete recorded a tombstone");

    // Warm the whole table from the source -> the tombstoned row must NOT come back.
    psql(&clone, "postgres", "SELECT gfs.warm('orders'::regclass)");
    assert_eq!(psql(&clone, "postgres", "SELECT count(*) FROM orders WHERE id=5"), "0",
        "tombstoned row NOT resurrected by warm");
    assert_eq!(psql(&clone, "postgres", "SELECT count(*) FROM orders"),
        (psql(remote, "shop", "SELECT count(*) FROM orders").parse::<i64>().unwrap() - 1).to_string(),
        "clone has every source row except the one deleted");
    assert_eq!(psql(remote, "shop", "SELECT count(*) FROM orders WHERE id=5"), "1", "source row untouched");
    drop(cl);
}
