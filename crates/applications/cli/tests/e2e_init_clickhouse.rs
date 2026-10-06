//! Integration tests for `gfs init` with ClickHouse.

mod common;

use std::path::PathBuf;
use std::process::Command;

use common::{cli_runner, clickhouse};
use gfs_domain::repo_utils::repo_layout::{
    get_environment_config, get_runtime_config, validate_repo_layout,
};
use serial_test::serial;

struct ContainerCleanupGuard(String);

impl Drop for ContainerCleanupGuard {
    fn drop(&mut self) {
        let _ = Command::new("docker").args(["stop", &self.0]).output();
        let _ = Command::new("docker").args(["rm", "-f", &self.0]).output();
    }
}

#[test]
#[serial]
fn gfs_init_clickhouse_creates_valid_repo_layout_and_runtime_config() {
    let temp_dir = common::shared_tempdir::shared_tempdir().expect("create temp dir");
    let work_dir = temp_dir.path().to_path_buf();

    let ok = cli_runner::gfs_init_with_provider(&work_dir, "clickhouse", "24.8.14.39");
    assert!(ok, "gfs init with clickhouse should succeed");

    let gfs_path: PathBuf = work_dir.join(".gfs");
    assert!(gfs_path.exists(), ".gfs directory should exist");
    assert!(gfs_path.is_dir(), ".gfs should be a directory");
    assert!(
        validate_repo_layout(&gfs_path).is_ok(),
        ".gfs layout should be valid"
    );

    let runtime = get_runtime_config(&work_dir)
        .expect("read runtime config")
        .expect("runtime config should exist");
    let _cleanup = ContainerCleanupGuard(runtime.container_name.clone());
    assert_eq!(runtime.runtime_provider, "docker");
    assert!(
        runtime.container_name.starts_with("gfs-clickhouse"),
        "container name should use clickhouse prefix; got: {}",
        runtime.container_name
    );
    assert!(
        clickhouse::wait_for_clickhouse(&runtime.container_name),
        "ClickHouse should stay up long enough to answer queries"
    );
    let version = clickhouse::run_clickhouse_query(&runtime.container_name, "SELECT version()");
    assert!(
        version.contains("24.8.14.39"),
        "expected ClickHouse version in query output; got: {version}"
    );

    let environment = get_environment_config(&work_dir)
        .expect("read environment config")
        .expect("environment config should exist");
    assert_eq!(environment.database_provider, "clickhouse");
    assert_eq!(environment.database_version, "24.8.14.39");
}
