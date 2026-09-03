//! End-to-end tests for the MCP server against the file-based SQLite provider.
//!
//! These exist because `skills/use-gfs-mcp/SKILL.md` tells an agent that the
//! MCP server supports "SQLite (version 3) — file-based; needs no container
//! runtime". Both halves of that sentence were false: every tool built a Docker
//! client before deciding whether it needed one, so with no daemon reachable
//! even `init --database-provider sqlite` failed; and `query` required a
//! runtime section in the config that an embedded repo never writes.
//!
//! The reader of that file is a machine, which cannot notice the discrepancy.
//! So the claim is pinned here instead: every test drives the real `gfs-mcp`
//! binary over stdio with `DOCKER_HOST` pointed at a socket that does not
//! exist, which is what makes a pass mean the tool genuinely reached the
//! database rather than quietly finding a daemon on the developer's machine.
//!
//! Unix, not macOS-only: nothing here is APFS-specific, and running on Linux
//! is what makes the claim mean anything on the platform most users deploy to.
//! Windows is out because the CLI suite beside it shells out to `chmod`, and
//! keeping the two gated alike avoids one passing where the other cannot.

#![cfg(unix)]

use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};

use serde_json::{Value, json};

/// A live `gfs-mcp` process speaking newline-delimited JSON-RPC over stdio.
struct McpSession {
    child: Child,
    stdin: ChildStdin,
    stdout: Box<dyn BufRead>,
    next_id: i64,
}

impl McpSession {
    /// Start the server with no reachable container runtime and complete the
    /// MCP handshake.
    fn start() -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_gfs-mcp"))
            // Any tool that still reaches for Docker fails loudly here rather
            // than silently succeeding against a daemon that happens to run.
            .env("DOCKER_HOST", "unix:///nonexistent/gfs-mcp-test.sock")
            .env_remove("GFS_RUNTIME_PROVIDER")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn gfs-mcp");

        let stdin = child.stdin.take().expect("stdin");
        let stdout = Box::new(BufReader::new(child.stdout.take().expect("stdout")));
        let mut session = Self {
            child,
            stdin,
            stdout,
            next_id: 0,
        };

        session.request(
            "initialize",
            json!({
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": { "name": "mcp_sqlite_no_runtime", "version": "1" },
            }),
        );
        writeln!(
            session.stdin,
            r#"{{"jsonrpc":"2.0","method":"notifications/initialized"}}"#
        )
        .expect("write initialized");
        session.stdin.flush().expect("flush");
        session
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        self.next_id += 1;
        let req = json!({
            "jsonrpc": "2.0",
            "id": self.next_id,
            "method": method,
            "params": params,
        });
        writeln!(self.stdin, "{req}").expect("write request");
        self.stdin.flush().expect("flush");

        let mut line = String::new();
        self.stdout.read_line(&mut line).expect("read response");
        serde_json::from_str(&line).unwrap_or_else(|e| panic!("parse {line:?}: {e}"))
    }

    /// Call a tool and return its payload, panicking with the server's own
    /// message if the call failed. A failure here is the whole point of the
    /// suite, so the message is worth surfacing verbatim.
    fn call(&mut self, tool: &str, args: Value) -> Value {
        let resp = self.request("tools/call", json!({ "name": tool, "arguments": args }));
        if let Some(err) = resp.get("error") {
            panic!(
                "tool '{tool}' failed: {}",
                err.get("message").and_then(Value::as_str).unwrap_or("?")
            );
        }
        let result = resp.get("result").expect("result");
        let text = result
            .get("content")
            .and_then(|c| c.get(0))
            .and_then(|c| c.get("text"))
            .and_then(Value::as_str)
            .unwrap_or("");
        assert!(
            result.get("isError").and_then(Value::as_bool) != Some(true),
            "tool '{tool}' reported an error: {text}"
        );
        serde_json::from_str(text).unwrap_or_else(|_| json!({ "text": text }))
    }

    /// Call a tool that is expected to fail, returning the message.
    fn call_expecting_error(&mut self, tool: &str, args: Value) -> String {
        let resp = self.request("tools/call", json!({ "name": tool, "arguments": args }));
        if let Some(err) = resp.get("error") {
            return err
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
        }
        let result = resp.get("result").expect("result");
        let text = result
            .get("content")
            .and_then(|c| c.get(0))
            .and_then(|c| c.get("text"))
            .and_then(Value::as_str)
            .unwrap_or("");
        assert_eq!(
            result.get("isError").and_then(Value::as_bool),
            Some(true),
            "expected '{tool}' to fail, got: {text}"
        );
        text.to_string()
    }
}

impl Drop for McpSession {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Seed a repo without the host `sqlite3` binary, by importing SQL through the
/// linked engine.
fn import_seed(session: &mut McpSession, repo: &Path, sql: &str) {
    let seed = repo.join("seed.sql");
    std::fs::write(&seed, sql).expect("write seed");
    session.call(
        "import_database",
        json!({ "path": repo, "file": seed, "format": "sql" }),
    );
}

fn init_sqlite(session: &mut McpSession, repo: &Path) {
    session.call(
        "init",
        json!({ "path": repo, "database_provider": "sqlite", "database_version": "3" }),
    );
}

/// `gfs query` shells out to the host client, which is the one part of this
/// surface that needs a binary the repository does not ship.
fn have_sqlite3() -> bool {
    Command::new("sqlite3")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
}

fn row_count(session: &mut McpSession, repo: &Path) -> i64 {
    let out = session.call(
        "query",
        json!({ "path": repo, "query": "SELECT count(*) FROM t;" }),
    );
    out.get("stdout")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .parse()
        .unwrap_or_else(|_| panic!("unparsable count: {out}"))
}

#[test]
fn the_repository_lifecycle_needs_no_container_runtime() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    let mut s = McpSession::start();

    init_sqlite(&mut s, repo);
    import_seed(
        &mut s,
        repo,
        "CREATE TABLE t(a INTEGER PRIMARY KEY, b TEXT);\nINSERT INTO t VALUES(1,'one'),(2,'two');\n",
    );

    // The schema came from the linked engine, not from a container.
    let schema = s.call("extract_schema", json!({ "path": repo }));
    let tables = serde_json::to_string(&schema).unwrap();
    assert!(
        tables.contains("\"t\""),
        "schema should mention table t: {tables}"
    );

    let commit = s.call("commit", json!({ "path": repo, "message": "seeded" }));
    let commit_id = commit
        .get("commit_id")
        .and_then(Value::as_str)
        .expect("commit_id")
        .to_string();

    let log = s.call("log", json!({ "path": repo }));
    let commits = log
        .get("commits")
        .and_then(Value::as_array)
        .expect("commits");
    assert_eq!(commits.len(), 1, "one commit expected: {log}");

    let status = s.call("status", json!({ "path": repo }));
    assert_eq!(
        status.get("current_branch").and_then(Value::as_str),
        Some("main")
    );

    // show_schema reads the committed snapshot, again with no runtime.
    let shown = s.call("show_schema", json!({ "path": repo, "commit": commit_id }));
    assert!(
        shown
            .get("ddl")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .contains("CREATE TABLE t"),
        "show_schema should return the DDL: {shown}"
    );

    let export = s.call("export_database", json!({ "path": repo, "format": "sql" }));
    let file = export
        .get("file_path")
        .and_then(Value::as_str)
        .expect("file_path");
    let dump = std::fs::read_to_string(file).expect("read dump");
    assert!(
        dump.contains("CREATE TABLE t") && dump.contains("'one'"),
        "dump should carry schema and rows: {dump}"
    );
}

#[test]
fn query_reaches_the_embedded_engine_and_time_travel_works() {
    if !have_sqlite3() {
        // Skipping is fine on a developer machine without the client, but in CI
        // a silent skip is a hole in exactly the coverage this suite exists to
        // provide — so there it is a failure. Both GitHub runners ship sqlite3.
        assert!(
            std::env::var("CI").is_err(),
            "CI must exercise the query path: host 'sqlite3' not found on PATH"
        );
        eprintln!("SKIP query_reaches_the_embedded_engine: host 'sqlite3' not found");
        return;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    let mut s = McpSession::start();

    init_sqlite(&mut s, repo);
    import_seed(
        &mut s,
        repo,
        "CREATE TABLE t(a INTEGER PRIMARY KEY, b TEXT);\nINSERT INTO t VALUES(1,'one'),(2,'two');\n",
    );
    let first = s
        .call("commit", json!({ "path": repo, "message": "two rows" }))
        .get("commit_id")
        .and_then(Value::as_str)
        .expect("commit_id")
        .to_string();

    // This is the call that used to answer "no runtime configured".
    assert_eq!(row_count(&mut s, repo), 2);

    s.call(
        "query",
        json!({ "path": repo, "query": "INSERT INTO t VALUES(3,'three');" }),
    );
    s.call("commit", json!({ "path": repo, "message": "three rows" }));
    assert_eq!(row_count(&mut s, repo), 3);

    s.call("checkout", json!({ "path": repo, "revision": first }));
    assert_eq!(
        row_count(&mut s, repo),
        2,
        "checkout should move the workspace back to the two-row commit"
    );

    s.call("checkout", json!({ "path": repo, "revision": "main" }));
    assert_eq!(row_count(&mut s, repo), 3, "and forward again");
}

#[test]
fn query_without_sql_describes_the_file_rather_than_a_host_and_port() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    let mut s = McpSession::start();
    init_sqlite(&mut s, repo);

    let info = s.call("query", json!({ "path": repo }));
    let conn = info.get("connection_info").expect("connection_info");
    assert_eq!(conn.get("provider").and_then(Value::as_str), Some("sqlite"));
    assert!(
        conn.get("connection_string")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .starts_with("sqlite:"),
        "expected a sqlite: URL: {conn}"
    );
    // An embedded provider has no address, so reporting ""/0 would be worse
    // than reporting nothing.
    assert!(conn.get("host").is_none(), "no host for a file: {conn}");
    assert!(conn.get("port").is_none(), "no port for a file: {conn}");
}

#[test]
fn query_rejects_a_database_argument_for_an_embedded_provider() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    let mut s = McpSession::start();
    init_sqlite(&mut s, repo);

    let msg = s.call_expecting_error(
        "query",
        json!({ "path": repo, "query": "SELECT 1;", "database": "other" }),
    );
    assert!(
        msg.contains("does not apply") && msg.contains("sqlite"),
        "should explain that a file has no database to select: {msg}"
    );
}

/// The status tool must return what the skill file says it returns.
///
/// `skills/use-gfs-mcp/SKILL.md` promises "current branch, HEAD commit, and
/// connection information". It returned `{compute: null, current_branch: ...}`
/// — two hand-picked fields, less than the CLI's own `--json status`, with
/// nothing telling the caller the rest were missing.
#[test]
fn status_returns_the_fields_its_documentation_promises() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    let mut s = McpSession::start();

    init_sqlite(&mut s, repo);
    import_seed(&mut s, repo, "CREATE TABLE t(a INTEGER PRIMARY KEY);\n");
    let commit = s.call("commit", json!({ "path": repo, "message": "one" }));
    let commit_id = commit
        .get("commit_id")
        .and_then(Value::as_str)
        .expect("commit_id")
        .to_string();

    let status = s.call("status", json!({ "path": repo }));
    assert_eq!(
        status.get("current_branch").and_then(Value::as_str),
        Some("main")
    );
    assert_eq!(
        status.get("head_commit").and_then(Value::as_str),
        Some(commit_id.as_str()),
        "HEAD is the field the skill names most specifically: {status}"
    );
    assert!(
        status
            .get("connection_string")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .starts_with("sqlite:"),
        "an embedded provider has no compute section for its connection string, \
         so it needs one of its own: {status}"
    );
    assert!(
        status.get("active_workspace_data_dir").is_some(),
        "the CLI reports this and MCP dropped it: {status}"
    );
}

/// MCP must refuse what the CLI refuses.
///
/// Provider and version were validated in `cmd_init.rs` only, so the MCP server
/// accepted `database_version: "4"` and wrote it to config.toml permanently
/// while the CLI rejected the same input — and an unknown provider name
/// reported a Docker daemon failure for a database that needs no daemon. The
/// authoritative check now lives in `InitRepositoryUseCase`, which both callers
/// reach; the check here only makes the error name the typo rather than Docker.
#[test]
fn init_refuses_what_the_cli_refuses() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut s = McpSession::start();

    let msg = s.call_expecting_error(
        "init",
        json!({
            "path": dir.path().join("bad-version"),
            "database_provider": "sqlite",
            "database_version": "4",
        }),
    );
    assert!(
        msg.contains("not a supported") && msg.contains("Supported: 3"),
        "should say what is supported: {msg}"
    );

    let msg = s.call_expecting_error(
        "init",
        json!({
            "path": dir.path().join("bad-provider"),
            "database_provider": "sqlite3",
            "database_version": "3",
        }),
    );
    assert!(
        msg.contains("unknown database provider") && msg.contains("sqlite3"),
        "should name the typo: {msg}"
    );
    assert!(
        !msg.contains("Docker") && !msg.contains("Podman"),
        "no container runtime is involved: {msg}"
    );

    // And the valid form still works.
    let repo = dir.path().join("good");
    s.call(
        "init",
        json!({ "path": repo, "database_provider": "sqlite", "database_version": "3" }),
    );
    assert!(repo.join(".gfs/config.toml").exists());
}

/// `status` reports the fields the CLI reports, from the shared response.
///
/// `head_commit` existed only as a field the MCP tool injected into its own
/// payload, so the CLI reported none in any output mode and the two skill files
/// documented the same command two different ways. It now lives on
/// `StatusResponse`, which both surfaces serialise. The CLI half of this parity
/// is asserted in `gfs-cli`'s `e2e_sqlite`, where that binary is available.
#[test]
fn status_reports_the_shared_response_fields() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    let mut s = McpSession::start();

    init_sqlite(&mut s, repo);
    import_seed(&mut s, repo, "CREATE TABLE t(a INTEGER PRIMARY KEY);\n");
    let commit = s.call("commit", json!({ "path": repo, "message": "one" }));
    let expected = commit
        .get("commit_id")
        .and_then(Value::as_str)
        .expect("commit_id");

    let from_mcp = s.call("status", json!({ "path": repo }));

    assert_eq!(
        from_mcp.get("head_commit").and_then(Value::as_str),
        Some(expected),
        "status must report the commit HEAD points at: {from_mcp}"
    );
    assert_eq!(
        from_mcp.get("current_branch").and_then(Value::as_str),
        Some("main")
    );
    assert!(
        from_mcp.get("active_workspace_data_dir").is_some(),
        "the field the CLI also reports must be here: {from_mcp}"
    );
}
