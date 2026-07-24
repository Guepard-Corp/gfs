//! Integration: `ManageUsersUseCase` grant / revoke / list_privileges run inside
//! a real Postgres container — the live proof for RFC 007 story 003. This drives
//! the REAL use case through the REAL `DockerCompute` (not the mock harness), so
//! it exercises resolve(.gfs) → provider SQL → `Compute::exec` → engine → map.
//!
//! Run: `GFS_DOCKER_IT=1 cargo test -p gfs-compute-docker --test manage_users_it -- --nocapture`

use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gfs_compute_docker::DockerCompute;
use gfs_compute_docker::containers;
use gfs_domain::model::config::{EnvironmentConfig, GfsConfig, RuntimeConfig};
use gfs_domain::model::db_user::{
    GrantSpec, GrantableObject, Privilege, RevokeSpec, RolePreset, RoleSpec,
};
use gfs_domain::ports::database_provider::InMemoryDatabaseProviderRegistry;
use gfs_domain::usecases::repository::manage_users_usecase::ManageUsersUseCase;

const CONTAINER: &str = "gfs-it-manage-users";

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
        remote: None,
    };
    config.save(path).expect("save config");
}

/// Independent verification: run scalar SQL directly via `docker exec` psql.
fn psql(sql: &str) -> String {
    let out = Command::new("docker")
        .args(["exec", CONTAINER, "psql", "-U", "postgres", "-tAc", sql])
        .output()
        .expect("docker exec psql");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn start_postgres() {
    let _ = Command::new("docker")
        .args(["rm", "-f", CONTAINER])
        .output();
    let status = Command::new("docker")
        .args([
            "run",
            "-d",
            "--name",
            CONTAINER,
            "-e",
            "POSTGRES_PASSWORD=postgres",
            "postgres:17",
        ])
        .status()
        .expect("docker run");
    assert!(status.success(), "docker run postgres:17 failed");

    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let ok = Command::new("docker")
            .args([
                "exec",
                CONTAINER,
                "pg_isready",
                "-U",
                "postgres",
                "-d",
                "postgres",
            ])
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if ok {
            break;
        }
        assert!(Instant::now() < deadline, "postgres did not become ready");
        std::thread::sleep(Duration::from_millis(500));
    }
}

fn stop_postgres() {
    let _ = Command::new("docker")
        .args(["rm", "-f", CONTAINER])
        .output();
}

#[tokio::test]
async fn grant_revoke_list_through_real_usecase() {
    if !docker_ok() {
        eprintln!("skip: set GFS_DOCKER_IT=1 and ensure docker is running");
        return;
    }

    start_postgres();
    // Fixtures: a client role + a table to grant on.
    psql("CREATE ROLE app_ro; CREATE TABLE public.t(id int);");

    let tmp = tempfile::tempdir().expect("tempdir");
    let repo = tmp.path();
    write_repo(repo, CONTAINER);

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
    let after_grant = psql("SELECT has_table_privilege('app_ro','public.t','SELECT')");

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
    let after_revoke = psql("SELECT has_table_privilege('app_ro','public.t','SELECT')");

    // Clean up BEFORE asserting so a failed assert never leaks the container.
    stop_postgres();

    assert_eq!(after_grant, "t", "SELECT must be granted after uc.grant");
    assert!(
        has_select,
        "uc.list_privileges must report the table SELECT grant; got: {privs:?}"
    );
    assert_eq!(after_revoke, "f", "SELECT must be gone after uc.revoke");
}

/// A preset's `ALTER DEFAULT PRIVILEGES` must be role-scoped to the deploy
/// `owner` so a preset user automatically sees tables the OWNER creates LATER —
/// not just the tables that existed at create time. This is the RFC-007 preset
/// hardening: `create_role` with `default_privileges_owner: Some("owner")` emits
/// `... FOR ROLE "owner" ...`. The `reader_self` role (owner `None`) is the
/// control: without `FOR ROLE`, a preset only covers the connecting admin's
/// future objects, so it must NOT see the owner's future table.
#[tokio::test]
async fn preset_default_privileges_follow_owner_future_objects() {
    if !docker_ok() {
        eprintln!("skip: set GFS_DOCKER_IT=1 and ensure docker is running");
        return;
    }

    start_postgres();
    // The customer's object-creating role (RFC 009 deploy owner), with CREATE on
    // `public` so it can make tables of its own.
    psql("CREATE ROLE owner LOGIN; GRANT CREATE ON SCHEMA public TO owner;");

    let tmp = tempfile::tempdir().expect("tempdir");
    let repo = tmp.path();
    write_repo(repo, CONTAINER);

    let compute = Arc::new(DockerCompute::new().expect("docker compute"));
    let registry = Arc::new(InMemoryDatabaseProviderRegistry::new());
    containers::register_all(&*registry).expect("register providers");
    let uc = ManageUsersUseCase::new(compute, registry);

    // reader: readonly preset whose defaults are role-scoped to `owner` (the fix).
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

    // reader_self: readonly preset with NO deploy owner — defaults role-scope to
    // the connecting admin (postgres), so they cannot cover owner's future objects.
    uc.create_role(
        repo,
        &RoleSpec {
            username: "reader_self".into(),
            password: "pw_reader_1234".into(),
            preset: Some(RolePreset::Readonly),
            default_privileges_owner: None,
        },
    )
    .await
    .expect("create reader_self");

    // The OWNER creates a FUTURE table (after both presets were applied).
    psql("SET ROLE owner; CREATE TABLE public.future_t(id int); RESET ROLE;");

    let reader_sees = psql("SELECT has_table_privilege('reader','public.future_t','SELECT')");
    let reader_self_sees =
        psql("SELECT has_table_privilege('reader_self','public.future_t','SELECT')");

    // Clean up BEFORE asserting so a failed assert never leaks the container.
    stop_postgres();

    assert_eq!(
        reader_sees, "t",
        "preset defaults FOR ROLE owner must auto-grant SELECT on owner's future table"
    );
    assert_eq!(
        reader_self_sees, "f",
        "control: without FOR ROLE owner, a preset must NOT cover the owner's future objects"
    );
}
