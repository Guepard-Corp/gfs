//! Integration tests for `gfs schema extract` with PostgreSQL.
//!
//! Run with: `cargo test -p gfs-cli schema_extract_postgres -- --test-threads=1`
//! Uses gfs_cli::run() in-process for coverage capture.

use std::fs;

#[path = "../common/mod.rs"]
mod common;

use common::postgres::{get_container_id, run_psql_select, with_fresh_repo};
use gfs_domain::model::datasource::DatasourceMetadata;
use serial_test::serial;

/// Extract schema from a running PostgreSQL database and verify the output.
#[test]
#[serial]
#[cfg(target_os = "macos")]
fn schema_extract_postgres() {
    with_fresh_repo(|repo_path| {
        // 1. Create table via docker exec
        let container_id = get_container_id(repo_path);
        run_psql_select(
            &container_id,
            "CREATE TABLE IF NOT EXISTS schema_test (id int, name text);",
        );

        // 2. Run gfs schema extract --output <path> (in-process for coverage)
        let output_path = repo_path.join("schema.json");
        let (ok, _, stderr) =
            common::cli_runner::gfs_schema_extract(repo_path, Some(&output_path), false);

        assert!(ok, "gfs schema extract should succeed; stderr: {stderr}");

        // 3. Parse JSON and verify schema
        let json = fs::read_to_string(&output_path).expect("read schema.json");
        let meta: DatasourceMetadata =
            serde_json::from_str(&json).expect("schema.json should be valid DatasourceMetadata");

        assert!(
            meta.version.starts_with("PostgreSQL"),
            "version should start with PostgreSQL; got: {}",
            meta.version
        );
        assert_eq!(meta.driver, "postgres");
        assert!(
            meta.schemas.iter().any(|s| s.name == "public"),
            "schemas should contain public; got: {:?}",
            meta.schemas
        );
        assert!(
            meta.tables.iter().any(|t| t.name == "schema_test"),
            "tables should contain schema_test; got: {:?}",
            meta.tables
        );
        assert!(
            meta.columns
                .iter()
                .any(|c| c.name == "id" && c.table == "schema_test"),
            "columns should contain id for schema_test; got: {:?}",
            meta.columns
        );
        assert!(
            meta.columns
                .iter()
                .any(|c| c.name == "name" && c.table == "schema_test"),
            "columns should contain name for schema_test; got: {:?}",
            meta.columns
        );
    });
}

/// Error case: running schema extract on a directory that is not a GFS repo.
#[test]
fn schema_extract_error_not_initialized() {
    let temp = tempfile::tempdir().expect("tempdir");
    let (ok, _, stderr) = common::cli_runner::gfs_schema_extract(temp.path(), None, false);

    assert!(!ok, "schema extract on non-repo should fail");
    assert!(
        stderr.contains("schema extraction failed")
            || stderr.contains("not configured")
            || stderr.contains("config"),
        "stderr should mention failure; got: {stderr}"
    );
}
