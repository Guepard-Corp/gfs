//! Use case for bootstrapping a lazy (copy-on-read) clone of a remote database.
//!
//! Unlike `import`, this copies **no data** up front. It sets up a
//! foreign-data-wrapper link plus mixed-partition tables in the already
//! provisioned local GFS database, so that data is fetched from the remote on
//! first read and served locally thereafter. See `docs/rfcs/008-remote-clone.md`.
//!
//! Orchestration (mirrors the export/import sidecar pattern):
//! 1. Load repo config to get the provider name and container name.
//! 2. Resolve the provider from the registry.
//! 3. Get the internal connection info the sidecar uses to reach the LOCAL db.
//! 4. Ask the provider for a `clone_bootstrap_spec` (sidecar definition + command).
//! 5. Run the bootstrap sidecar linked to the local database instance.

use std::path::Path;
use std::sync::Arc;

use crate::model::config::GfsConfig;
use crate::ports::compute::{Compute, ComputeError, EnvVar, InstanceId};
use crate::ports::database_provider::{ConnectionParams, DatabaseProviderRegistry, RemoteSource};

// ---------------------------------------------------------------------------
// Error
// ---------------------------------------------------------------------------

#[derive(Debug, thiserror::Error)]
pub enum CloneRepoError {
    #[error("repository not configured for compute: {0}")]
    NotConfigured(String),

    #[error("database provider not found: '{0}'")]
    ProviderNotFound(String),

    #[error("provider does not support lazy clone: {0}")]
    Unsupported(String),

    #[error(transparent)]
    Compute(#[from] ComputeError),

    #[error("clone bootstrap failed (exit {exit_code}): {stderr}")]
    TaskFailed { exit_code: i32, stderr: String },

    #[error("config error: {0}")]
    Config(String),
}

// ---------------------------------------------------------------------------
// Output
// ---------------------------------------------------------------------------

/// Result of a successful clone bootstrap.
pub struct CloneOutput {
    /// Remote host:port that was cloned from.
    pub remote: String,
    /// Stdout captured from the bootstrap sidecar.
    pub stdout: String,
    /// Stderr captured from the bootstrap sidecar.
    pub stderr: String,
}

// ---------------------------------------------------------------------------
// Use case
// ---------------------------------------------------------------------------

pub struct CloneRepoUseCase<R: DatabaseProviderRegistry> {
    compute: Arc<dyn Compute>,
    registry: Arc<R>,
}

/// Build the shell that launches the in-pod bootstrap.
///
/// Shared by `run_task_impl` and its tests on purpose. It used to be built inline
/// and the tests kept a second, identical copy of this `format!` -- so they
/// asserted on the copy and passed even when the real launcher changed. Removing
/// `setsid` from production left both green.
///
/// `umask 077` and the `rm -f` matter: the body carries the SOURCE database
/// password in cleartext (postgresql.rs builds the libpq password variable into
/// the command), and this directory lives inside PGDATA, which `gfs commit`
/// snapshots. So the script is written owner-only and removed the moment the
/// work ends -- on the failure path too, where the scratch dir is otherwise kept
/// for a later repair. The log and sentinel, which carry no secret, are what
/// that repair reads.
///
/// This narrows the exposure; it does not end it. A lazy clone keeps a live FDW
/// link to the source, so Postgres stores that password in the user mapping for
/// `gfs_remote_srv` in order to reconnect, and the catalog write lands in the
/// WAL. Measured after a real clone: no file under PGDATA contained the string
/// `PGPASSWORD`, but `pg_wal/…0001` still contained the source password itself
/// (mode 0600). Getting it out of the snapshot entirely means detaching the FDW
/// after seeding, not tightening this script.
///
/// The pid recorded is the WORKER's (`$!`), not the wrapping shell's. Recording
/// `$$` named the shell that merely waits, so killing it reported the bootstrap
/// gone while the real work carried on writing DDL.
fn bootstrap_launch_script(dir: &str, body: &str) -> String {
    let script = format!("{dir}/bootstrap.sh");
    let status = format!("{dir}/status");
    let log = format!("{dir}/bootstrap.log");
    let pid = format!("{dir}/pid");
    format!(
        "mkdir -p {dir}\n\
         rm -f {status}\n\
         umask 077\n\
         printf '%s' {b} > {script}\n\
         setsid sh -c 'sh {script} > {log} 2>&1 & echo $! > {pid}; wait $!; \
rc=$?; rm -f {script}; echo $rc > {status}' \
           >/dev/null 2>&1 </dev/null &\n\
         exit 0",
        b = crate::utils::shell::shell_single_quote(body),
    )
}

impl<R: DatabaseProviderRegistry> CloneRepoUseCase<R> {
    pub fn new(compute: Arc<dyn Compute>, registry: Arc<R>) -> Self {
        Self { compute, registry }
    }

    /// Probe the remote PostgreSQL for its **major** version (e.g. `"16"`), so
    /// the clone can provision a matching local engine instead of a default.
    ///
    /// Runs a one-off sidecar `psql ... SHOW server_version_num` against the
    /// remote (password via `PGPASSWORD`, never on the command line).
    pub async fn detect_remote_version(
        &self,
        remote: &RemoteSource,
    ) -> Result<String, CloneRepoError> {
        let provider = self
            .registry
            .get("postgres")
            .ok_or_else(|| CloneRepoError::ProviderNotFound("postgres".into()))?;

        let container = provider
            .require_container()
            .map_err(|e| CloneRepoError::Unsupported(e.to_string()))?;
        let mut def = container.definition();
        def.env = vec![EnvVar {
            name: "PGPASSWORD".into(),
            default: Some(remote.password.clone()),
        }];
        if let Some(sslmode) = &remote.sslmode {
            def.env.push(EnvVar {
                name: "PGSSLMODE".into(),
                default: Some(sslmode.clone()),
            });
        }
        def.ports = vec![];
        def.host_data_dir = None;
        def.user = None;

        // Quoted for the same reason as the clone bootstrap's dump: these come from
        // a user-supplied `--from` URL and end up inside `sh -c` in a task pod.
        let cmd = format!(
            "psql -h {} -p {} -U {} -d {} -tAc 'SHOW server_version_num'",
            crate::utils::shell::shell_single_quote(&remote.host),
            remote.port,
            crate::utils::shell::shell_single_quote(&remote.user),
            crate::utils::shell::shell_single_quote(&remote.dbname),
        );

        let out = self.compute.run_task(&def, &cmd, None).await?;
        if out.exit_code != 0 {
            return Err(CloneRepoError::Config(format!(
                "remote version probe failed (exit {}): {}",
                out.exit_code,
                out.stderr.trim()
            )));
        }
        let num: u32 = out.stdout.trim().parse().map_err(|_| {
            CloneRepoError::Config(format!(
                "unexpected server_version_num from remote: '{}'",
                out.stdout.trim()
            ))
        })?;
        Ok((num / 10000).to_string())
    }

    /// Bootstrap a lazy clone of `remote` into the local GFS database at `path`.
    ///
    /// The local repository must already be initialised and its database
    /// container running (e.g. via `gfs init --database-provider postgres ...`).
    pub async fn run(
        &self,
        path: &Path,
        remote: RemoteSource,
    ) -> Result<CloneOutput, CloneRepoError> {
        // 1. Load repo config.
        let config = GfsConfig::load(path).map_err(|e| CloneRepoError::Config(e.to_string()))?;

        let provider_name = config
            .environment
            .as_ref()
            .map(|e| e.database_provider.as_str())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                CloneRepoError::NotConfigured(
                    "no database provider configured (run gfs init --database-provider <name>)"
                        .into(),
                )
            })?
            .to_string();

        let container_name = config
            .runtime
            .as_ref()
            .map(|r| r.container_name.as_str())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                CloneRepoError::NotConfigured(
                    "no container configured (run gfs compute start)".into(),
                )
            })?
            .to_string();

        // 2. Resolve provider.
        let provider = self
            .registry
            .get(&provider_name)
            .ok_or_else(|| CloneRepoError::ProviderNotFound(provider_name.clone()))?;

        let container = provider
            .require_container()
            .map_err(|e| CloneRepoError::Unsupported(e.to_string()))?;

        let instance_id = InstanceId(container_name);

        // 3. The bootstrap runs INSIDE the database container, so it reaches the
        //    server over loopback -- which the startup seal already trusts
        //    unconditionally (`local all all trust`, `host all all 127.0.0.1/32
        //    trust`). No pg_hba rule is written at any point, which is the whole
        //    reason this moved in-pod: a source-address allowance needs a
        //    family-aware prefix, a gateway guard, an unforgeable identity and a
        //    removal that survives process death, and none of that exists here
        //    because nothing is authorized by address (decision-3).
        //
        //    The env still comes from the instance so the credentials match.
        let conn_info = self
            .compute
            .get_task_connection_info(&instance_id, container.default_port())
            .await?;

        let local = ConnectionParams {
            host: "127.0.0.1".to_string(),
            port: container.default_port(),
            env: conn_info.env,
        };

        let remote_label = format!("{}:{}", remote.host, remote.port);

        // 4. Build the bootstrap spec.
        let mut spec = container
            .clone_bootstrap_spec(&local, &remote)
            .map_err(|e| CloneRepoError::Unsupported(e.to_string()))?;
        spec.definition.image = crate::usecases::repository::task_image::task_image_for_version(
            &spec.definition.image,
            &config,
        );

        // 5. Run the bootstrap in the instance's own container -- but DETACHED, and
        //    poll for its result, rather than holding it open on the exec stream.
        //
        //    `exec` is a WebSocket attach with no lifecycle: run the bootstrap on
        //    it directly and the work dies with the connection. Measured on the dev
        //    cluster -- killing the daemon mid-bootstrap orphaned the in-pod process
        //    and kubelet reaped it part-way, leaving 403 foreign tables registered
        //    but the row mapping never completed, so reads returned zero rows.
        //
        //    `setsid` detaches it from the exec session, so it survives the stream
        //    closing and runs to completion on its own. Its exit code lands in a
        //    sentinel and its output in a log, both on the instance's data volume,
        //    so a caller that dies mid-bootstrap leaves the evidence behind for a
        //    later repair instead of losing it. Verified on a live pod: a detached
        //    job outlived the exec that launched it and wrote its sentinel.
        let dir = &spec.scratch_dir;
        let status = format!("{dir}/status");
        let log = format!("{dir}/bootstrap.log");
        // Liveness is checked by pid, not by name. `pgrep -f bootstrap.sh` matches
        // the very shell running the probe -- the pattern is inside the command
        // string -- so it reports RUNNING forever and the "gone" branch never fires.
        let pid = format!("{dir}/pid");

        let launch = bootstrap_launch_script(dir, &spec.command);
        let launched = self.compute.exec(&instance_id, &launch, None).await?;
        if launched.exit_code != 0 {
            return Err(CloneRepoError::TaskFailed {
                exit_code: launched.exit_code,
                stderr: format!("could not launch bootstrap: {}", launched.stderr.trim()),
            });
        }

        // Poll the sentinel. The bootstrap's own wait-for-server loop budgets up to
        // ~120s before it even starts work, so this window has to exceed it.
        //    Poll for the sentinel OR the disappearance of the process. A bare
        //    timeout is not enough: if the launch silently started nothing, waiting
        //    the whole window before saying so buries the real failure under a
        //    timeout that looks like slowness. "No sentinel and nothing running"
        //    is a definite answer and is reported immediately.
        let mut code: Option<i32> = None;
        let mut vanished = false;
        for _ in 0..900 {
            let probe = self
                .compute
                .exec(
                    &instance_id,
                    &format!(
                        "if [ -s {status} ]; then cat {status}; \
                         elif [ -f {pid} ] && kill -0 \"$(cat {pid})\" 2>/dev/null; \
                         then echo RUNNING; else echo ABSENT; fi"
                    ),
                    None,
                )
                .await?;
            let seen = probe.stdout.trim().to_string();
            if let Ok(c) = seen.parse::<i32>() {
                code = Some(c);
                break;
            }
            if seen == "ABSENT" {
                vanished = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        }

        let logs = self
            .compute
            .exec(&instance_id, &format!("cat {log} 2>/dev/null"), None)
            .await
            .map(|o| o.stdout)
            .unwrap_or_default();

        let Some(exit_code) = code else {
            // Leave the scratch dir in place either way: the bootstrap may still be
            // running, and its log is the only record of how far it got.
            let why = if vanished {
                "bootstrap process is gone and wrote no exit status (it was killed, \
                 or never started)"
            } else {
                "bootstrap did not finish within 900s"
            };
            return Err(CloneRepoError::TaskFailed {
                exit_code: -1,
                stderr: format!("{why}; log so far:\n{logs}"),
            });
        };

        // Read first, then clean up -- a caller that dies before this point leaves
        // the sentinel and log for a repair to find.
        let _ = self
            .compute
            .exec(&instance_id, &format!("rm -rf {dir}"), None)
            .await;

        // Reassemble the shape the failure handling below already expects.
        let output = crate::ports::compute::ExecOutput {
            exit_code,
            stdout: logs,
            stderr: String::new(),
        };

        if output.exit_code != 0 {
            let stderr = output.stderr.trim();
            let stdout = output.stdout.trim();
            let detail = if stderr.is_empty() {
                stdout.to_string()
            } else if stdout.is_empty() {
                stderr.to_string()
            } else {
                format!("{stderr}\n{stdout}")
            };
            return Err(CloneRepoError::TaskFailed {
                exit_code: output.exit_code,
                stderr: detail,
            });
        }

        Ok(CloneOutput {
            remote: remote_label,
            stdout: output.stdout,
            stderr: output.stderr,
        })
    }
}

#[cfg(test)]
mod tests {
    /// The launcher and poll are built inline in `run`, so this pins their shape
    /// from the one place that is testable without a live pod: the strings
    /// themselves. Behaviour is covered end-to-end on a cluster.
    /// Calls the SAME builder production uses. It previously kept its own copy of
    /// the format string, so it pinned the copy rather than the launcher.
    fn launcher_for(dir: &str, body: &str) -> String {
        super::bootstrap_launch_script(dir, body)
    }

    #[test]
    fn the_bootstrap_is_launched_detached_so_it_outlives_the_connection() {
        let dir = "/var/lib/postgresql/data/.gfs_bootstrap";
        let cmd = launcher_for(dir, "echo hi");
        // `setsid` is the whole point: without it the work dies with the exec
        // stream. Measured on a live cluster -- killing the daemon while the
        // bootstrap ran on the stream left it reaped part-way, with foreign tables
        // registered but the row mapping never completed.
        assert!(
            cmd.contains("setsid "),
            "must detach from the exec session: {cmd}"
        );
        assert!(
            cmd.contains("</dev/null"),
            "must not hold stdin open: {cmd}"
        );
        assert!(
            cmd.trim_end().ends_with("exit 0"),
            "launch must return immediately: {cmd}"
        );
        // The exit code has to outlive the process that produced it.
        assert!(
            cmd.contains("rc=$?") && cmd.contains("echo $rc > "),
            "must record an exit status: {cmd}"
        );
        assert!(
            cmd.contains("bootstrap.log"),
            "must capture output for a later repair: {cmd}"
        );
        // Liveness must be answerable by pid. Checking by name (`pgrep -f
        // bootstrap.sh`) matches the probe's own shell, because the pattern is in
        // the command string -- so it reports RUNNING forever and the "gone" branch
        // is dead code. Measured: a reap guarded that way never fired.
        assert!(
            cmd.contains("echo $! > ") && cmd.contains("/pid"),
            "the detached run must record its pid so liveness is not checked by name: {cmd}"
        );
        // ...and it must be the WORKER's pid, not the wrapping shell's. `$$` named
        // the shell that only waits, so killing it reported the bootstrap gone
        // while the orphaned worker carried on writing DDL.
        assert!(
            !cmd.contains("echo $$ > "),
            "must not record the wrapper's pid -- liveness would not follow the work: {cmd}"
        );
        assert!(
            cmd.contains("wait $!"),
            "the wrapper must wait on the worker so the sentinel is the worker's status: {cmd}"
        );
        // The body carries the source password and this dir is inside PGDATA,
        // which gfs commit snapshots. Owner-only, and gone when the work ends.
        assert!(
            cmd.contains("umask 077"),
            "the script carrying the source password must be owner-only: {cmd}"
        );
        // Contiguous, and the script path specifically: `rm -f <dir>/status` and
        // the `printf ... > <dir>/bootstrap.sh` both already appear, so asserting
        // on "rm -f" and "bootstrap.sh" separately passes with no removal at all.
        assert!(
            cmd.contains(&format!("rm -f {dir}/bootstrap.sh")),
            "the credential-bearing script must be removed when the work ends: {cmd}"
        );
    }

    #[test]
    fn a_body_with_quotes_survives_being_shipped_into_the_pod() {
        // The bootstrap SQL is full of single quotes; the launcher embeds it in a
        // single-quoted shell word, so the escaping has to hold or the script that
        // lands in the pod is not the one we built.
        let body = "psql -c 'SELECT ''a'' FROM t' && echo done";
        let cmd = launcher_for("/d", body);
        let quoted = crate::utils::shell::shell_single_quote(body);
        assert!(
            cmd.contains(&quoted),
            "body must be shell-quoted verbatim: {cmd}"
        );
        assert!(
            !cmd.contains("SELECT ''a'' FROM t &&"),
            "must not leak unquoted: {cmd}"
        );
    }
}
