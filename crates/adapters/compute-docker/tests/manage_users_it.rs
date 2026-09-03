//! Integration: `ManageUsersUseCase` grant / revoke / list_privileges run inside
//! a real Postgres container — the live proof for the object-level grant work.
//! This drives the REAL use case through the REAL `DockerCompute` (not the mock
//! harness), so it exercises resolve(.gfs) → provider SQL → `Compute::exec` →
//! engine → map.
//!
//! Run: `GFS_DOCKER_IT=1 cargo test -p gfs-compute-docker --test manage_users_it -- --nocapture`

use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use gfs_compute_docker::DockerCompute;
use gfs_compute_docker::containers;
use gfs_domain::model::config::{EnvironmentConfig, GfsConfig, RuntimeConfig};
use gfs_domain::model::db_user::{
    GrantSpec, GrantableObject, Privilege, RevokeSpec, RolePreset, RoleSpec,
};
use gfs_domain::ports::database_provider::InMemoryDatabaseProviderRegistry;
use gfs_domain::usecases::repository::manage_users_usecase::ManageUsersUseCase;

/// A container name unique to each call (pid + counter) so tests never collide,
/// whether run in parallel or back-to-back within one test binary.
fn unique_container() -> String {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    format!(
        "gfs-it-manage-users-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    )
}

/// Owns a throwaway `postgres:17` container and removes it on drop, so a failed
/// assertion (panic unwind) can never leak the container.
struct Postgres {
    container: String,
}

impl Postgres {
    fn start() -> Self {
        let container = unique_container();
        start_postgres(&container);
        Self { container }
    }

    fn psql(&self, sql: &str) -> String {
        psql(&self.container, sql)
    }

    fn name(&self) -> &str {
        &self.container
    }
}

impl Drop for Postgres {
    fn drop(&mut self) {
        stop_postgres(&self.container);
    }
}

fn docker_ok() -> bool {
    std::env::var("GFS_DOCKER_IT").ok().as_deref() == Some("1")
        && Command::new("docker")
            .args(["info"])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
}

fn write_repo(path: &std::path::Path, container: &str) {
    std::fs::create_dir_all(path.join(".gfs")).expect("mkdir .gfs");
    let config = GfsConfig {
        mount_point: None,
        version: String::new(),
        description: String::new(),
        user: None,
        environment: Some(EnvironmentConfig {
            database_provider: "postgres".into(),
            database_version: "17".into(),
            database_port: None,
            display_name: None,
        }),
        runtime: Some(RuntimeConfig {
            runtime_provider: "docker".into(),
            runtime_version: "latest".into(),
            container_name: container.into(),
        }),
        storage: None,
        compute: None,
    };
    config.save(path).expect("save config");
}

/// Independent verification: run scalar SQL directly via `docker exec` psql.
fn psql(container: &str, sql: &str) -> String {
    let out = Command::new("docker")
        .args(["exec", container, "psql", "-U", "postgres", "-tAc", sql])
        .output()
        .expect("docker exec psql");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn start_postgres(container: &str) {
    let _ = Command::new("docker")
        .args(["rm", "-f", container])
        .output();
    let status = Command::new("docker")
        .args([
            "run",
            "-d",
            "--name",
            container,
            "-e",
            "POSTGRES_PASSWORD=postgres",
            "postgres:17",
        ])
        .status()
        .expect("docker run");
    assert!(status.success(), "docker run postgres:17 failed");

    // Gate on a real `SELECT 1` over the SAME channel the use case uses —
    // TCP `-h 127.0.0.1` with the password — not the unix socket. The postgres
    // image runs a temporary socket-only server during init, so a socket probe
    // (or `pg_isready`) reports ready while TCP is still refused; the use case
    // connects over TCP and would race "connection refused" / "starting up".
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let ready = Command::new("docker")
            .args([
                "exec",
                "-e",
                "PGPASSWORD=postgres",
                container,
                "psql",
                "-h",
                "127.0.0.1",
                "-U",
                "postgres",
                "-d",
                "postgres",
                "-tAc",
                "SELECT 1",
            ])
            .output()
            .map(|o| o.status.success() && String::from_utf8_lossy(&o.stdout).trim() == "1")
            .unwrap_or(false);
        if ready {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "postgres did not accept queries in time"
        );
        std::thread::sleep(Duration::from_millis(500));
    }
}

fn stop_postgres(container: &str) {
    let _ = Command::new("docker")
        .args(["rm", "-f", container])
        .output();
}

#[tokio::test]
async fn grant_revoke_list_through_real_usecase() {
    if !docker_ok() {
        eprintln!("skip: set GFS_DOCKER_IT=1 and ensure docker is running");
        return;
    }

    let pg = Postgres::start();
    // Fixtures: a client role + a table to grant on.
    pg.psql("CREATE ROLE app_ro; CREATE TABLE public.t(id int);");

    let tmp = tempfile::tempdir().expect("tempdir");
    let repo = tmp.path();
    write_repo(repo, pg.name());

    let compute = Arc::new(DockerCompute::new().expect("docker compute"));
    let registry = Arc::new(InMemoryDatabaseProviderRegistry::new());
    containers::register_all(&*registry).expect("register providers");
    let uc = ManageUsersUseCase::new(compute, registry);

    let table = || GrantableObject::Table {
        schema: "public".into(),
        name: "t".into(),
    };

    // GRANT SELECT via the real use case, then verify the engine actually changed.
    uc.grant(
        repo,
        &GrantSpec {
            role: "app_ro".into(),
            object: table(),
            privileges: vec![Privilege::Select],
            with_grant_option: false,
            apply_to_future: None,
        },
    )
    .await
    .expect("uc.grant");
    let after_grant = pg.psql("SELECT has_table_privilege('app_ro','public.t','SELECT')");

    // LIST via the real use case — parses the live engine catalog JSON into
    // Vec<ObjectPrivilege> (the 4-way UNION projection end-to-end).
    let privs = uc
        .list_privileges(repo, "app_ro")
        .await
        .expect("uc.list_privileges");
    let has_select = privs.iter().any(|p| {
        p.object_type == "table" && p.object_name == "public.t" && p.privilege == "select"
    });

    // REVOKE via the real use case, then verify it's gone.
    uc.revoke(
        repo,
        &RevokeSpec {
            role: "app_ro".into(),
            object: table(),
            privileges: vec![Privilege::Select],
            cascade: false,
        },
    )
    .await
    .expect("uc.revoke");
    let after_revoke = pg.psql("SELECT has_table_privilege('app_ro','public.t','SELECT')");

    // The container is removed by `pg`'s Drop on scope exit — including on a
    // failed assertion below (panic unwind), so nothing leaks either way.
    assert_eq!(after_grant, "t", "SELECT must be granted after uc.grant");
    assert!(
        has_select,
        "uc.list_privileges must report the table SELECT grant; got: {privs:?}"
    );
    assert_eq!(after_revoke, "f", "SELECT must be gone after uc.revoke");
}

/// A preset's `ALTER DEFAULT PRIVILEGES` must be role-scoped to the deploy
/// `owner` so a preset user automatically sees tables the OWNER creates LATER —
/// not just the tables that existed at create time. Under the group model this
/// scoping is a property of the preset GROUP (`ALTER DEFAULT PRIVILEGES FOR ROLE
/// owner ... TO gfs_readonly`), set once and shared by every member — a database
/// has a single deploy owner, so all its readonly users correctly see the
/// owner's future objects via inherited group SELECT. The control is the *level*:
/// readonly confers SELECT, never INSERT, even on the owner's future table.
#[tokio::test]
async fn preset_default_privileges_follow_owner_future_objects() {
    if !docker_ok() {
        eprintln!("skip: set GFS_DOCKER_IT=1 and ensure docker is running");
        return;
    }

    let pg = Postgres::start();
    // The customer's object-creating deploy owner, with CREATE on `public` so it
    // can make tables of its own.
    pg.psql("CREATE ROLE owner LOGIN; GRANT CREATE ON SCHEMA public TO owner;");

    let tmp = tempfile::tempdir().expect("tempdir");
    let repo = tmp.path();
    write_repo(repo, pg.name());

    let compute = Arc::new(DockerCompute::new().expect("docker compute"));
    let registry = Arc::new(InMemoryDatabaseProviderRegistry::new());
    containers::register_all(&*registry).expect("register providers");
    let uc = ManageUsersUseCase::new(compute, registry);

    // reader: readonly preset whose group defaults are role-scoped to `owner`.
    uc.create_role(
        repo,
        &RoleSpec {
            username: "reader".into(),
            password: "pw_reader_1234".into(),
            preset: Some(RolePreset::Readonly),
            default_privileges_owner: Some("owner".into()),
        },
    )
    .await
    .expect("create reader");

    // The OWNER creates a FUTURE table (after the preset was applied).
    pg.psql("SET ROLE owner; CREATE TABLE public.future_t(id int); RESET ROLE;");

    let reader_selects = pg.psql("SELECT has_table_privilege('reader','public.future_t','SELECT')");
    let reader_inserts = pg.psql("SELECT has_table_privilege('reader','public.future_t','INSERT')");

    assert_eq!(
        reader_selects, "t",
        "readonly group defaults FOR ROLE owner must auto-grant SELECT on owner's future table"
    );
    assert_eq!(
        reader_inserts, "f",
        "readonly level must never confer INSERT, even on the owner's future table"
    );
}

/// `apply_preset` must be DECLARATIVE, not additive: downgrading a role from
/// `readwrite` to `readonly` must actually REVOKE the write privileges, not leave
/// them (an HTTP-200 downgrade that silently keeps INSERT/UPDATE/DELETE is a
/// security-correctness bug). Proven at the engine with `has_table_privilege`.
#[tokio::test]
async fn apply_preset_downgrade_revokes_write_privileges() {
    if !docker_ok() {
        eprintln!("skip: set GFS_DOCKER_IT=1 and ensure docker is running");
        return;
    }

    let pg = Postgres::start();
    // Owner + a pre-existing table the presets act on.
    pg.psql(
        "CREATE ROLE owner LOGIN; GRANT CREATE ON SCHEMA public TO owner; CREATE TABLE public.t(id int);",
    );

    let tmp = tempfile::tempdir().expect("tempdir");
    let repo = tmp.path();
    write_repo(repo, pg.name());

    let compute = Arc::new(DockerCompute::new().expect("docker compute"));
    let registry = Arc::new(InMemoryDatabaseProviderRegistry::new());
    containers::register_all(&*registry).expect("register providers");
    let uc = ManageUsersUseCase::new(compute, registry);

    // Create a readwrite user (write access), then DOWNGRADE to readonly.
    uc.create_role(
        repo,
        &RoleSpec {
            username: "u".into(),
            password: "pw_downgrade_1234".into(),
            preset: Some(RolePreset::Readwrite),
            default_privileges_owner: Some("owner".into()),
        },
    )
    .await
    .expect("create readwrite");
    let insert_before = pg.psql("SELECT has_table_privilege('u','public.t','INSERT')");

    uc.apply_preset(repo, "u", RolePreset::Readonly, Some("owner"))
        .await
        .expect("apply readonly");
    let insert_after = pg.psql("SELECT has_table_privilege('u','public.t','INSERT')");
    let select_after = pg.psql("SELECT has_table_privilege('u','public.t','SELECT')");

    assert_eq!(insert_before, "t", "readwrite preset must grant INSERT");
    assert_eq!(
        insert_after, "f",
        "downgrade to readonly must REVOKE INSERT (declarative, not additive)"
    );
    assert_eq!(select_after, "t", "readonly must still allow SELECT");
}

/// Attempt a password login as `user`, returning whether it authenticated.
/// Connects via the container's **non-loopback** address on purpose: the postgres
/// image's `pg_hba.conf` has `host all all 127.0.0.1/32 trust` (loopback ignores
/// the password), so a loopback probe cannot tell a right password from a wrong
/// one — the customer-facing `host all all all scram-sha-256` rule (which enforces
/// the password) matches the eth0 address instead.
fn login(container: &str, user: &str, password: &str) -> bool {
    let cmd = format!(
        "PGPASSWORD='{password}' psql -h \"$(hostname -i)\" -U '{user}' -d postgres -tAc 'SELECT 1'"
    );
    let out = Command::new("docker")
        .args(["exec", container, "sh", "-c", &cmd])
        .output()
        .expect("docker exec psql login");
    out.status.success() && String::from_utf8_lossy(&out.stdout).trim() == "1"
}

/// Run one statement AS a managed client user (over the customer-facing scram
/// endpoint, not the mgmt loopback), returning whether it SUCCEEDED. A
/// permission-denied error makes psql (with `ON_ERROR_STOP`) exit non-zero, so a
/// rejected escape reads as `false`.
fn as_user_ok(container: &str, user: &str, password: &str, sql: &str) -> bool {
    let cmd = format!(
        "PGPASSWORD='{password}' psql -h \"$(hostname -i)\" -U '{user}' -d postgres -v ON_ERROR_STOP=1 -c \"{sql}\""
    );
    Command::new("docker")
        .args(["exec", container, "sh", "-c", &cmd])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// The reserved-group fence, verified (not enforced via an in-DB trigger — Postgres
/// event triggers do NOT fire on GRANT / role DDL). A managed customer user is
/// NOSUPERUSER NOCREATEROLE and holds its preset-group membership WITHOUT admin
/// option, so it cannot escape the platform-managed level via raw SQL: it can
/// neither drop nor alter a reserved group role, grant itself into a higher group,
/// create roles, nor self-escalate to superuser. "Membership == level" is only
/// trustworthy because these attempts are denied by the standard privilege system.
#[tokio::test]
async fn a_managed_user_cannot_escape_the_reserved_group_fence() {
    if !docker_ok() {
        eprintln!("skip: set GFS_DOCKER_IT=1 and ensure docker is running");
        return;
    }

    let pg = Postgres::start();
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo = tmp.path();
    write_repo(repo, pg.name());

    let compute = Arc::new(DockerCompute::new().expect("docker compute"));
    let registry = Arc::new(InMemoryDatabaseProviderRegistry::new());
    containers::register_all(&*registry).expect("register providers");
    let uc = ManageUsersUseCase::new(compute, registry);

    // A managed readonly user — the platform grants it INTO gfs_readonly.
    uc.create_role(
        repo,
        &RoleSpec {
            username: "app".into(),
            password: "app_pw_1234".into(),
            preset: Some(RolePreset::Readonly),
            default_privileges_owner: None,
        },
    )
    .await
    .expect("create app");

    // Fence preconditions: not privileged, and the group membership has no ADMIN OPTION.
    assert_eq!(
        pg.psql("SELECT (rolsuper OR rolcreaterole OR rolcreatedb)::text FROM pg_roles WHERE rolname='app'"),
        "false",
        "a managed user is never SUPERUSER / CREATEROLE / CREATEDB"
    );
    assert_eq!(
        pg.psql("SELECT COALESCE(bool_or(admin_option), false)::text FROM pg_auth_members m JOIN pg_roles g ON g.oid = m.roleid JOIN pg_roles u ON u.oid = m.member WHERE u.rolname='app' AND g.rolname IN ('gfs_readonly','gfs_readwrite','gfs_admin')"),
        "false",
        "the preset-group membership carries no ADMIN OPTION"
    );
    assert!(
        login(pg.name(), "app", "app_pw_1234"),
        "app can log in (baseline)"
    );

    // Positive control: the SAME harness runs a permitted statement AS app and it
    // succeeds — proving `as_user_ok` genuinely executes and distinguishes allow from
    // deny, so the rejected escapes below are real denials, not a dead connection.
    assert!(
        as_user_ok(pg.name(), "app", "app_pw_1234", "SELECT 1"),
        "a permitted statement AS app must succeed"
    );

    // Every privilege-escalation attempt, run AS app, must be REJECTED.
    let escapes = [
        "DROP ROLE gfs_readonly",
        "ALTER ROLE gfs_admin NOLOGIN",
        "GRANT gfs_admin TO app",
        "GRANT gfs_readwrite TO app",
        "CREATE ROLE evil LOGIN PASSWORD 'x'",
        "ALTER ROLE app SUPERUSER",
        "ALTER ROLE app CREATEROLE",
    ];
    for sql in escapes {
        assert!(
            !as_user_ok(pg.name(), "app", "app_pw_1234", sql),
            "the reserved-group fence must reject: {sql}"
        );
    }

    // The fence held: reserved groups intact, app not escalated, no new role created.
    assert_eq!(
        pg.psql("SELECT count(*)::text FROM pg_roles WHERE rolname IN ('gfs_readonly','gfs_readwrite','gfs_admin')"),
        "3",
        "the reserved group roles are intact"
    );
    assert_eq!(
        pg.psql("SELECT rolsuper::text FROM pg_roles WHERE rolname='app'"),
        "false",
        "app did not self-escalate to superuser"
    );
    assert_eq!(
        pg.psql("SELECT pg_has_role('app','gfs_admin','MEMBER')::text"),
        "false",
        "app did not grant itself into gfs_admin"
    );
    assert_eq!(
        pg.psql("SELECT count(*)::text FROM pg_roles WHERE rolname='evil'"),
        "0",
        "app created no roles (NOCREATEROLE)"
    );
}

/// Session termination for immediate revocation: `terminate_user_sessions` kills a
/// managed user's live backends (returning the count), and `drop_role` bakes the
/// same in so a dropped user's open connection dies at once instead of lingering
/// until it happens to disconnect.
#[tokio::test]
async fn terminate_user_sessions_kills_live_backends_and_drop_bakes_it_in() {
    if !docker_ok() {
        eprintln!("skip: set GFS_DOCKER_IT=1 and ensure docker is running");
        return;
    }

    let pg = Postgres::start();
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo = tmp.path();
    write_repo(repo, pg.name());

    let compute = Arc::new(DockerCompute::new().expect("docker compute"));
    let registry = Arc::new(InMemoryDatabaseProviderRegistry::new());
    containers::register_all(&*registry).expect("register providers");
    let mu = ManageUsersUseCase::new(compute, registry);

    mu.create_role(
        repo,
        &RoleSpec {
            username: "app".into(),
            password: "app_pw_1234".into(),
            preset: None,
            default_privileges_owner: None,
        },
    )
    .await
    .expect("create app");

    // No sessions yet → terminate is a safe no-op returning 0.
    assert_eq!(
        mu.terminate_user_sessions(repo, "app")
            .await
            .expect("terminate (none)"),
        0,
        "no live backends to terminate initially"
    );

    // Open two held sessions as app (backgrounded pg_sleep over the scram endpoint).
    let mut s1 = spawn_held_session(pg.name(), "app", "app_pw_1234");
    let mut s2 = spawn_held_session(pg.name(), "app", "app_pw_1234");
    wait_for_session_count(pg.name(), "app", 2, true);

    // Explicit terminate kills both and reports the count.
    let killed = mu
        .terminate_user_sessions(repo, "app")
        .await
        .expect("terminate (two)");
    assert!(killed >= 2, "terminated app's live backends, got {killed}");
    wait_for_session_count(pg.name(), "app", 0, false);
    let _ = s1.kill();
    let _ = s1.wait();
    let _ = s2.kill();
    let _ = s2.wait();

    // set_password (rotation) bakes in termination too: a session authenticated
    // with the OLD credential dies when the password is rotated.
    let mut s_rot = spawn_held_session(pg.name(), "app", "app_pw_1234");
    wait_for_session_count(pg.name(), "app", 1, true);
    mu.set_password(repo, "app", "app_pw_ROTATED_9999")
        .await
        .expect("rotate app");
    wait_for_session_count(pg.name(), "app", 0, false);
    let _ = s_rot.kill();
    let _ = s_rot.wait();

    // drop_role bakes it in: a held session dies when the role is dropped.
    let mut s3 = spawn_held_session(pg.name(), "app", "app_pw_ROTATED_9999");
    wait_for_session_count(pg.name(), "app", 1, true);
    mu.drop_role(repo, "app").await.expect("drop app");
    wait_for_session_count(pg.name(), "app", 0, false);
    let _ = s3.kill();
    let _ = s3.wait();

    // Reserved roles are never terminated (fail-closed).
    assert!(
        mu.terminate_user_sessions(repo, "owner").await.is_err(),
        "terminate refuses reserved roles"
    );
}

/// Hold a live backend open as `user` for ~30s (backgrounded `pg_sleep`) over the
/// container's scram endpoint, so a test can observe it being terminated.
fn spawn_held_session(container: &str, user: &str, password: &str) -> std::process::Child {
    let inner = format!(
        "PGPASSWORD='{password}' psql -h \"$(hostname -i)\" -U '{user}' -d postgres -c 'SELECT pg_sleep(30)'"
    );
    Command::new("docker")
        .args(["exec", container, "sh", "-c", &inner])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn held session")
}

/// Count `user`'s live backends via the management (superuser) loopback.
fn app_session_count(container: &str, user: &str) -> u64 {
    let sql = format!("SELECT count(*) FROM pg_stat_activity WHERE usename='{user}'");
    let inner = format!(
        "PGPASSWORD=\"${{POSTGRES_PASSWORD:-postgres}}\" psql -h 127.0.0.1 -U \"${{POSTGRES_USER:-postgres}}\" -d postgres -tAc \"{sql}\""
    );
    let out = Command::new("docker")
        .args(["exec", container, "sh", "-c", &inner])
        .output()
        .expect("count sessions");
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse()
        .unwrap_or(0)
}

/// Poll until `user`'s backend count reaches `want` (when `at_least`, count >=
/// want; else count <= want). Backends appear/disappear a beat after connect /
/// terminate, so this tolerates that lag.
fn wait_for_session_count(container: &str, user: &str, want: u64, at_least: bool) {
    for _ in 0..50 {
        let n = app_session_count(container, user);
        if (at_least && n >= want) || (!at_least && n <= want) {
            return;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    panic!(
        "session count for {user} never {} {want} (last={})",
        if at_least { ">=" } else { "<=" },
        app_session_count(container, user)
    );
}
