//! Integration: repo.query path runs SQL inside the DB container (no host psql).
//!
//! Run: `GFS_DOCKER_IT=1 cargo test -p gfs-compute-docker --test query_in_instance_it -- --nocapture`

use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gfs_compute_docker::containers;
use gfs_compute_docker::DockerCompute;
use gfs_domain::model::config::{EnvironmentConfig, GfsConfig, RuntimeConfig};
use gfs_domain::ports::database_provider::InMemoryDatabaseProviderRegistry;
use gfs_domain::usecases::repository::execute_query_usecase::ExecuteQueryUseCase;

const CONTAINER: &str = "gfs-it-query-exec";

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
        }),
        runtime: Some(RuntimeConfig {
            runtime_provider: "docker".into(),
            runtime_version: "latest".into(),
            container_name: container.into(),
        }),
        storage: None,
        remote: None,
    };
    config.save(path).expect("save config");
}

fn start_postgres_container() {
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
        assert!(
            Instant::now() < deadline,
            "postgres did not become ready in time"
        );
        std::thread::sleep(Duration::from_millis(500));
    }
}

fn stop_postgres_container() {
    let _ = Command::new("docker")
        .args(["rm", "-f", CONTAINER])
        .output();
}

#[tokio::test]
async fn execute_query_inside_running_postgres_container() {
    if !docker_ok() {
        eprintln!("skip: set GFS_DOCKER_IT=1 and ensure docker is running");
        return;
    }

    start_postgres_container();
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo_path = tmp.path();
    write_repo(repo_path, CONTAINER);

    let compute = Arc::new(DockerCompute::new().expect("docker compute"));
    let registry = Arc::new(InMemoryDatabaseProviderRegistry::new());
    containers::register_all(&*registry).expect("register providers");

    let uc = ExecuteQueryUseCase::new(compute, registry);
    let out = uc
        .run(repo_path, "SELECT 42 AS answer;")
        .await
        .expect("query inside container");

    assert!(
        out.stdout.contains("42"),
        "expected query result in stdout, got: {:?}",
        out.stdout
    );

    stop_postgres_container();
}
