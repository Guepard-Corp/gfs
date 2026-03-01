//! Integration tests for `gfs import` with PostgreSQL SQL format.
//!
//! Run with: `cargo test -p gfs-cli import_postgres_sql -- --test-threads=1`

#![cfg(target_os = "macos")]

use std::fs;

#[path = "../../common/mod.rs"]
mod common;

use common::postgres::*;
use serial_test::serial;

/// Import a plain SQL file and verify the data is loaded.
#[test]
#[serial]
fn import_postgres_sql() {
    with_fresh_repo(|repo_path| {
        let import_dir = repo_path.join("import_data");
        fs::create_dir_all(&import_dir).expect("create import dir");
        let sql_path = import_dir.join("schema_and_data.sql");
        fs::write(
            &sql_path,
            r#"
CREATE TABLE IF NOT EXISTS sql_import (id int, label text);
INSERT INTO sql_import (id, label) VALUES (10, 'ten'), (20, 'twenty');
"#,
        )
        .expect("write SQL");

        let (ok, stdout, stderr) = gfs_import(repo_path, &sql_path, Some("sql"));
        assert!(ok, "gfs import sql should succeed; stderr: {stderr}");
        if !stdout.is_empty() {
            assert!(
                stdout.contains("Imported from"),
                "stdout should mention import; got: {stdout}"
            );
        }

        let container_id = get_container_id(repo_path);
        let result = run_psql_select(
            &container_id,
            "SELECT id, label FROM sql_import ORDER BY id",
        );
        assert!(
            result.contains("10|ten"),
            "sql_import should contain (10,ten); got: {result}"
        );
        assert!(
            result.contains("20|twenty"),
            "sql_import should contain (20,twenty); got: {result}"
        );
    });
}
