//! A Docker `init` whose start fails after the container was created.
//!
//! Docker creates the container at provision time and allocates its host port
//! only at start. Publishing the requested port from another container first
//! makes that start fail with "port is already allocated", the same shape as a
//! start that fails for any other reason once the instance exists.
//!
//! Before the fix, the repository named the container only after a successful
//! start: `init` failed, `destroy` then reported success and removed `.gfs`,
//! and the created container was left behind with nothing pointing at it.
//!
//! Needs a container runtime and the locally built `gfs-postgres:17` image;
//! skipped (not failed) without them.

mod common;

use std::path::Path;

use common::cli_runner::run_gfs;
use common::container_runtime::runtime_command;
use common::shared_tempdir::shared_tempdir;

const IMAGE: &str = "gfs-postgres:17";

/// Stops (and so removes, it runs with `--rm`) the container holding the port,
/// and removes the database container if the test failed before destroy did.
struct Cleanup {
    port_holder: String,
    database: Option<String>,
}

impl Drop for Cleanup {
    fn drop(&mut self) {
        let _ = runtime_command()
            .args(["stop", "-t", "0", &self.port_holder])
            .output();
        if let Some(database) = &self.database {
            let _ = runtime_command().args(["rm", "-f", database]).output();
        }
    }
}

fn image_available() -> bool {
    runtime_command()
        .args(["image", "inspect", IMAGE])
        .output()
        .map(|out| out.status.success())
        .unwrap_or(false)
}

fn container_exists(name: &str) -> bool {
    runtime_command()
        .args(["container", "inspect", name])
        .output()
        .map(|out| out.status.success())
        .unwrap_or(false)
}

/// The containers the runtime holds for `repo`, found by the `gfs.repo` label
/// gfs puts on every container it provisions, whatever the repository records.
fn containers_labelled_for(repo: &str) -> Vec<String> {
    let out = runtime_command()
        .args([
            "ps",
            "-a",
            "--filter",
            &format!("label=gfs.repo={repo}"),
            "--format",
            "{{.Names}}",
        ])
        .output()
        .expect("list containers");
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::to_string)
        .collect()
}

/// Starts a container that publishes a runtime-chosen host port and returns
/// its id and that port.
fn hold_a_host_port() -> (String, String) {
    let run = runtime_command()
        .args([
            "run",
            "-d",
            "--rm",
            "-p",
            "5432",
            "--entrypoint",
            "sleep",
            IMAGE,
            "600",
        ])
        .output()
        .expect("run the port holder");
    assert!(
        run.status.success(),
        "port holder did not start: {}",
        String::from_utf8_lossy(&run.stderr)
    );
    let id = String::from_utf8_lossy(&run.stdout).trim().to_string();
    let port = runtime_command()
        .args(["port", &id, "5432"])
        .output()
        .expect("read the held port");
    let mapping = String::from_utf8_lossy(&port.stdout);
    let host_port = mapping
        .lines()
        .next()
        .and_then(|line| line.rsplit(':').next())
        .expect("a published port")
        .trim()
        .to_string();
    (id, host_port)
}

fn recorded_container_name(repo: &Path) -> Option<String> {
    let config = std::fs::read_to_string(repo.join(".gfs/config.toml")).ok()?;
    config.lines().find_map(|line| {
        let value = line.trim().strip_prefix("container_name")?;
        Some(
            value
                .trim_start_matches([' ', '='])
                .trim()
                .trim_matches('"')
                .to_string(),
        )
    })
}

#[test]
fn a_failed_start_leaves_a_container_that_destroy_removes() {
    if !image_available() {
        eprintln!("SKIP: image {IMAGE} absent or no container runtime");
        return;
    }
    let (port_holder, port) = hold_a_host_port();
    let mut cleanup = Cleanup {
        port_holder,
        database: None,
    };
    let dir = shared_tempdir().expect("temp dir");
    let repo = dir.path().to_str().expect("utf-8 path");

    let (ok, _, stderr) = run_gfs([
        "gfs",
        "init",
        "--database-provider",
        "postgres",
        "--database-version",
        "17",
        "--port",
        &port,
        repo,
    ]);
    assert!(!ok, "init must fail: port {port} is already published");
    assert!(
        stderr.contains("port is already allocated"),
        "init failed for another reason: {stderr}"
    );

    let created = containers_labelled_for(repo);
    assert_eq!(
        created.len(),
        1,
        "expected one container for {repo}: {created:?}"
    );
    let container = created[0].clone();
    cleanup.database = Some(container.clone());
    assert_eq!(
        recorded_container_name(dir.path()).as_deref(),
        Some(container.as_str()),
        "the failed init does not record the container it created"
    );

    let (ok, _, stderr) = run_gfs(["gfs", "destroy", "--path", repo, "-y"]);
    assert!(ok, "destroy failed: {stderr}");
    assert!(
        !container_exists(&container),
        "destroy left the container {container} behind"
    );
}
