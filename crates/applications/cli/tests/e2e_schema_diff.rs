#![cfg(target_os = "macos")]

mod common;

use common::cli_runner;
use gfs_domain::repo_utils::repo_layout;
use tempfile::tempdir;

#[test]
fn schema_diff_agentic_format_default() {
    let tmp = tempdir().unwrap();
    let repo_path = tmp.path();

    // Initialize repo with PostgreSQL
    let (ok, _, _) = cli_runner::run_gfs([
        "gfs",
        "init",
        "--database-provider",
        "postgres",
        "--database-version",
        "17",
        repo_path.to_str().unwrap(),
    ]);
    assert!(ok, "gfs init failed");

    // Wait for container to be ready
    std::thread::sleep(std::time::Duration::from_secs(5));

    // Create initial schema
    let (ok, _, _) = cli_runner::run_gfs([
        "gfs",
        "query",
        "--path",
        repo_path.to_str().unwrap(),
        "CREATE TABLE users (id INT PRIMARY KEY, name VARCHAR(100));",
    ]);
    assert!(ok, "CREATE TABLE users failed");

    // Commit
    let (ok, _, _) = cli_runner::run_gfs([
        "gfs",
        "commit",
        "--path",
        repo_path.to_str().unwrap(),
        "-m",
        "initial schema",
    ]);
    assert!(ok, "first commit failed");

    let hash1 = repo_layout::get_current_commit_id(repo_path).unwrap();

    // Add another table
    let (ok, _, _) = cli_runner::run_gfs([
        "gfs",
        "query",
        "--path",
        repo_path.to_str().unwrap(),
        "CREATE TABLE orders (id INT PRIMARY KEY, user_id INT);",
    ]);
    assert!(ok, "CREATE TABLE orders failed");

    // Commit
    let (ok, _, _) = cli_runner::run_gfs([
        "gfs",
        "commit",
        "--path",
        repo_path.to_str().unwrap(),
        "-m",
        "add orders table",
    ]);
    assert!(ok, "second commit failed");

    let hash2 = repo_layout::get_current_commit_id(repo_path).unwrap();

    // Run diff (default agentic format)
    let (ok, stdout, _) = cli_runner::run_gfs([
        "gfs",
        "schema",
        "diff",
        &hash1[..7],
        &hash2[..7],
        "--path",
        repo_path.to_str().unwrap(),
    ]);

    assert!(ok, "gfs schema diff failed");
    assert!(
        stdout.starts_with("GFS_DIFF v1"),
        "Expected GFS_DIFF header, got: {}",
        stdout
    );
    assert!(
        stdout.contains("TABLE ADD"),
        "Expected TABLE ADD in output: {}",
        stdout
    );
    assert!(
        stdout.contains("orders"),
        "Expected 'orders' table in output: {}",
        stdout
    );
}

#[test]
fn schema_diff_pretty_format() {
    let tmp = tempdir().unwrap();
    let repo_path = tmp.path();

    // Initialize repo with PostgreSQL
    let (ok, _, _) = cli_runner::run_gfs([
        "gfs",
        "init",
        "--database-provider",
        "postgres",
        "--database-version",
        "17",
        repo_path.to_str().unwrap(),
    ]);
    assert!(ok, "gfs init failed");

    // Wait for container to be ready
    std::thread::sleep(std::time::Duration::from_secs(5));

    // Create initial schema
    let (ok, _, _) = cli_runner::run_gfs([
        "gfs",
        "query",
        "--path",
        repo_path.to_str().unwrap(),
        "CREATE TABLE users (id INT PRIMARY KEY);",
    ]);
    assert!(ok, "CREATE TABLE failed");

    // Commit
    let (ok, _, _) = cli_runner::run_gfs([
        "gfs",
        "commit",
        "--path",
        repo_path.to_str().unwrap(),
        "-m",
        "initial",
    ]);
    assert!(ok, "commit failed");

    let hash1 = repo_layout::get_current_commit_id(repo_path).unwrap();

    // Add column
    let (ok, _, _) = cli_runner::run_gfs([
        "gfs",
        "query",
        "--path",
        repo_path.to_str().unwrap(),
        "ALTER TABLE users ADD COLUMN email VARCHAR(255);",
    ]);
    assert!(ok, "ALTER TABLE failed");

    // Commit
    let (ok, _, _) = cli_runner::run_gfs([
        "gfs",
        "commit",
        "--path",
        repo_path.to_str().unwrap(),
        "-m",
        "add email",
    ]);
    assert!(ok, "commit failed");

    let hash2 = repo_layout::get_current_commit_id(repo_path).unwrap();

    // Run diff with --pretty flag
    let (ok, stdout, _) = cli_runner::run_gfs([
        "gfs",
        "schema",
        "diff",
        &hash1[..7],
        &hash2[..7],
        "--pretty",
        "--no-color",
        "--path",
        repo_path.to_str().unwrap(),
    ]);

    assert!(ok, "gfs schema diff --pretty failed");
    assert!(
        stdout.contains("Schema diff"),
        "Expected 'Schema diff' header: {}",
        stdout
    );
    assert!(stdout.contains("→"), "Expected arrow symbol: {}", stdout);
    assert!(stdout.contains("━"), "Expected border line: {}", stdout);
    assert!(stdout.contains("+"), "Expected + (ADD) symbol: {}", stdout);
}

#[test]
fn schema_diff_no_changes_exit_code() {
    let tmp = tempdir().unwrap();
    let repo_path = tmp.path();

    // Initialize repo
    let (ok, _, _) = cli_runner::run_gfs([
        "gfs",
        "init",
        "--database-provider",
        "postgres",
        "--database-version",
        "17",
        repo_path.to_str().unwrap(),
    ]);
    assert!(ok);

    std::thread::sleep(std::time::Duration::from_secs(5));

    // Create and commit
    let (ok, _, _) = cli_runner::run_gfs([
        "gfs",
        "query",
        "--path",
        repo_path.to_str().unwrap(),
        "CREATE TABLE test (id INT);",
    ]);
    assert!(ok);

    let (ok, _, _) = cli_runner::run_gfs([
        "gfs",
        "commit",
        "--path",
        repo_path.to_str().unwrap(),
        "-m",
        "test",
    ]);
    assert!(ok);

    let hash = repo_layout::get_current_commit_id(repo_path).unwrap();

    // Diff same commit should have exit code 0
    let (ok, stdout, _) = cli_runner::run_gfs([
        "gfs",
        "schema",
        "diff",
        &hash[..7],
        &hash[..7],
        "--path",
        repo_path.to_str().unwrap(),
    ]);

    // Exit code 0 means ok=true in our test runner
    assert!(
        ok,
        "Expected exit code 0 for no changes, got stdout: {}",
        stdout
    );
}

#[test]
fn schema_diff_no_color_flag() {
    let tmp = tempdir().unwrap();
    let repo_path = tmp.path();

    // Initialize and setup
    let (ok, _, _) = cli_runner::run_gfs([
        "gfs",
        "init",
        "--database-provider",
        "postgres",
        "--database-version",
        "17",
        repo_path.to_str().unwrap(),
    ]);
    assert!(ok);

    std::thread::sleep(std::time::Duration::from_secs(5));

    // Create schema v1
    let (ok, _, _) = cli_runner::run_gfs([
        "gfs",
        "query",
        "--path",
        repo_path.to_str().unwrap(),
        "CREATE TABLE test (id INT);",
    ]);
    assert!(ok);

    let (ok, _, _) = cli_runner::run_gfs([
        "gfs",
        "commit",
        "--path",
        repo_path.to_str().unwrap(),
        "-m",
        "v1",
    ]);
    assert!(ok);
    let hash1 = repo_layout::get_current_commit_id(repo_path).unwrap();

    // Create schema v2
    let (ok, _, _) = cli_runner::run_gfs([
        "gfs",
        "query",
        "--path",
        repo_path.to_str().unwrap(),
        "ALTER TABLE test ADD COLUMN name VARCHAR(50);",
    ]);
    assert!(ok);

    let (ok, _, _) = cli_runner::run_gfs([
        "gfs",
        "commit",
        "--path",
        repo_path.to_str().unwrap(),
        "-m",
        "v2",
    ]);
    assert!(ok);
    let hash2 = repo_layout::get_current_commit_id(repo_path).unwrap();

    // Run with --pretty --no-color
    let (ok, stdout, _) = cli_runner::run_gfs([
        "gfs",
        "schema",
        "diff",
        &hash1[..7],
        &hash2[..7],
        "--pretty",
        "--no-color",
        "--path",
        repo_path.to_str().unwrap(),
    ]);

    assert!(ok);
    // Output should not contain ANSI color codes
    assert!(
        !stdout.contains("\x1b["),
        "Output should not contain ANSI codes with --no-color: {}",
        stdout
    );
}

#[test]
fn schema_diff_json_format() {
    let tmp = tempdir().unwrap();
    let repo_path = tmp.path();

    // Initialize repo with PostgreSQL
    let (ok, _, _) = cli_runner::run_gfs([
        "gfs",
        "init",
        "--database-provider",
        "postgres",
        "--database-version",
        "17",
        repo_path.to_str().unwrap(),
    ]);
    assert!(ok, "gfs init failed");

    // Wait for container to be ready
    std::thread::sleep(std::time::Duration::from_secs(5));

    // Create initial schema
    let (ok, _, _) = cli_runner::run_gfs([
        "gfs",
        "query",
        "--path",
        repo_path.to_str().unwrap(),
        "CREATE TABLE users (id INT PRIMARY KEY, name VARCHAR(100));",
    ]);
    assert!(ok, "CREATE TABLE users failed");

    // Commit
    let (ok, _, _) = cli_runner::run_gfs([
        "gfs",
        "commit",
        "--path",
        repo_path.to_str().unwrap(),
        "-m",
        "initial schema",
    ]);
    assert!(ok, "first commit failed");

    let hash1 = repo_layout::get_current_commit_id(repo_path).unwrap();

    // Add another table
    let (ok, _, _) = cli_runner::run_gfs([
        "gfs",
        "query",
        "--path",
        repo_path.to_str().unwrap(),
        "CREATE TABLE orders (id INT PRIMARY KEY, user_id INT);",
    ]);
    assert!(ok, "CREATE TABLE orders failed");

    // Commit
    let (ok, _, _) = cli_runner::run_gfs([
        "gfs",
        "commit",
        "--path",
        repo_path.to_str().unwrap(),
        "-m",
        "add orders table",
    ]);
    assert!(ok, "second commit failed");

    let hash2 = repo_layout::get_current_commit_id(repo_path).unwrap();

    // Run diff with --json flag
    let (ok, stdout, _) = cli_runner::run_gfs([
        "gfs",
        "schema",
        "diff",
        &hash1[..7],
        &hash2[..7],
        "--json",
        "--path",
        repo_path.to_str().unwrap(),
    ]);

    assert!(ok, "gfs schema diff --json failed");

    // Parse JSON output
    let json: serde_json::Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|_| panic!("Failed to parse JSON output: {}", stdout));

    // Verify JSON structure
    assert_eq!(json["version"], "1", "Expected version field");
    assert_eq!(json["from_commit"], hash1, "Expected from_commit field");
    assert_eq!(json["to_commit"], hash2, "Expected to_commit field");
    assert_eq!(
        json["has_breaking_changes"], false,
        "Expected no breaking changes"
    );
    assert_eq!(
        json["exit_code"], 1,
        "Expected exit code 1 for safe changes"
    );

    // Verify mutations array exists and has content
    let mutations = json["mutations"]
        .as_array()
        .expect("mutations should be an array");
    assert!(!mutations.is_empty(), "Expected at least one mutation");

    // Find the TABLE ADD mutation
    let table_add = mutations
        .iter()
        .find(|m| m["entity"] == "Table" && m["operation"] == "Add")
        .expect("Expected TABLE ADD mutation");

    assert!(
        table_add["target"].as_str().unwrap().contains("orders"),
        "Expected orders table in mutations"
    );
    assert_eq!(table_add["is_breaking"], false);

    // Verify summary
    assert!(
        json["summary"]["total"].as_u64().unwrap() > 0,
        "Expected non-zero total mutations"
    );
    assert!(
        json["summary"]["by_operation"].is_object(),
        "Expected by_operation object"
    );
    assert!(
        json["summary"]["by_entity"].is_object(),
        "Expected by_entity object"
    );
}

#[test]
fn schema_diff_json_no_changes() {
    let tmp = tempdir().unwrap();
    let repo_path = tmp.path();

    // Initialize repo
    let (ok, _, _) = cli_runner::run_gfs([
        "gfs",
        "init",
        "--database-provider",
        "postgres",
        "--database-version",
        "17",
        repo_path.to_str().unwrap(),
    ]);
    assert!(ok);

    std::thread::sleep(std::time::Duration::from_secs(5));

    // Create and commit
    let (ok, _, _) = cli_runner::run_gfs([
        "gfs",
        "query",
        "--path",
        repo_path.to_str().unwrap(),
        "CREATE TABLE test (id INT);",
    ]);
    assert!(ok);

    let (ok, _, _) = cli_runner::run_gfs([
        "gfs",
        "commit",
        "--path",
        repo_path.to_str().unwrap(),
        "-m",
        "test",
    ]);
    assert!(ok);

    let hash = repo_layout::get_current_commit_id(repo_path).unwrap();

    // Diff same commit with --json
    let (ok, stdout, _) = cli_runner::run_gfs([
        "gfs",
        "schema",
        "diff",
        &hash[..7],
        &hash[..7],
        "--json",
        "--path",
        repo_path.to_str().unwrap(),
    ]);

    assert!(ok, "Expected exit code 0 for no changes");

    // Parse JSON
    let json: serde_json::Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|_| panic!("Failed to parse JSON: {}", stdout));

    assert_eq!(json["version"], "1");
    assert_eq!(json["exit_code"], 0);
    assert_eq!(json["has_breaking_changes"], false);
    assert_eq!(
        json["mutations"].as_array().unwrap().len(),
        0,
        "Expected empty mutations array"
    );
    assert_eq!(json["summary"]["total"], 0, "Expected zero total mutations");
}

#[test]
fn schema_diff_json_pretty_mutually_exclusive() {
    let tmp = tempdir().unwrap();
    let repo_path = tmp.path();

    // Initialize repo
    let (ok, _, _) = cli_runner::run_gfs([
        "gfs",
        "init",
        "--database-provider",
        "postgres",
        "--database-version",
        "17",
        repo_path.to_str().unwrap(),
    ]);
    assert!(ok);

    std::thread::sleep(std::time::Duration::from_secs(5));

    // Create schema
    let (ok, _, _) = cli_runner::run_gfs([
        "gfs",
        "query",
        "--path",
        repo_path.to_str().unwrap(),
        "CREATE TABLE test (id INT);",
    ]);
    assert!(ok);

    let (ok, _, _) = cli_runner::run_gfs([
        "gfs",
        "commit",
        "--path",
        repo_path.to_str().unwrap(),
        "-m",
        "v1",
    ]);
    assert!(ok);
    let hash1 = repo_layout::get_current_commit_id(repo_path).unwrap();

    // Modify schema
    let (ok, _, _) = cli_runner::run_gfs([
        "gfs",
        "query",
        "--path",
        repo_path.to_str().unwrap(),
        "ALTER TABLE test ADD COLUMN name VARCHAR(50);",
    ]);
    assert!(ok);

    let (ok, _, _) = cli_runner::run_gfs([
        "gfs",
        "commit",
        "--path",
        repo_path.to_str().unwrap(),
        "-m",
        "v2",
    ]);
    assert!(ok);
    let hash2 = repo_layout::get_current_commit_id(repo_path).unwrap();

    // Try using --json and --pretty together (should fail)
    let (ok, _, stderr) = cli_runner::run_gfs([
        "gfs",
        "schema",
        "diff",
        &hash1[..7],
        &hash2[..7],
        "--json",
        "--pretty",
        "--path",
        repo_path.to_str().unwrap(),
    ]);

    assert!(
        !ok,
        "Expected command to fail with both --json and --pretty"
    );
    assert!(
        stderr.contains("--pretty and --json cannot be used together")
            || stderr.contains("mutually exclusive"),
        "Expected error about mutual exclusivity, got: {}",
        stderr
    );
}
