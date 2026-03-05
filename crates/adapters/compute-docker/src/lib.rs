//! Docker adapter for the [`Compute`] port.
//!
//! Implements lifecycle management of Docker containers via the Bollard async
//! client, which connects to the local Docker daemon over the Unix socket (or
//! named pipe on Windows) by default.
//!
//! # Limitations
//!
//! * Log timestamps are taken from the Docker log frame header (when
//!   `timestamps=true`); otherwise the current UTC time is used as a fallback.

pub mod containers;
mod error;

use async_trait::async_trait;
use futures_util::{StreamExt, TryStreamExt};
use gfs_domain::ports::compute::{
    Compute, ComputeDefinition, ComputeError, ExecOutput, InstanceConnectionInfo, InstanceId,
    InstanceState, InstanceStatus, LogEntry, LogStream, LogsOptions, Result, StartOptions,
};
use tracing::instrument;

use crate::error::classify;

// ---------------------------------------------------------------------------
// DockerCompute
// ---------------------------------------------------------------------------

/// Compute adapter backed by a local Docker daemon via [`bollard`].
#[derive(Debug, Clone)]
pub struct DockerCompute {
    docker: bollard::Docker,
}

impl DockerCompute {
    /// Connect to the local Docker daemon using platform defaults
    /// (`/var/run/docker.sock` on Unix, named pipe on Windows).
    ///
    /// Returns an error if the socket cannot be opened (e.g. Docker is not
    /// running or the socket path has wrong permissions).
    pub fn new() -> std::result::Result<Self, bollard::errors::Error> {
        match bollard::Docker::connect_with_local_defaults() {
            Ok(docker) => Ok(Self { docker }),
            Err(default_err) => {
                if let Some(socket_path) = Self::podman_socket_path() {
                    let socket = socket_path.to_string_lossy();

                    if let Ok(docker) = bollard::Docker::connect_with_unix(
                        socket.as_ref(),
                        120,
                        bollard::API_DEFAULT_VERSION,
                    ) {
                        return Ok(Self { docker });
                    }

                    let socket_uri = format!("unix://{}", socket);
                    if let Ok(docker) = bollard::Docker::connect_with_unix(
                        &socket_uri,
                        120,
                        bollard::API_DEFAULT_VERSION,
                    ) {
                        return Ok(Self { docker });
                    }
                }

                Err(default_err)
            }
        }
    }

    fn podman_socket_path() -> Option<std::path::PathBuf> {
        let from_xdg = std::env::var_os("XDG_RUNTIME_DIR")
            .map(std::path::PathBuf::from)
            .map(|dir| dir.join("podman").join("podman.sock"));

        if let Some(path) = from_xdg
            && path.exists()
        {
            return Some(path);
        }

        if let Some(uid) = std::env::var_os("UID") {
            let path = std::path::PathBuf::from(format!(
                "/run/user/{}/podman/podman.sock",
                uid.to_string_lossy()
            ));
            if path.exists() {
                return Some(path);
            }
        }

        None
    }

    async fn bind_mount_spec(&self, host_path: &str, container_path: &str) -> String {
        if self.is_podman_engine().await {
            // Podman commonly requires SELinux relabeling and uid/gid remapping for bind mounts.
            // Keep this Podman-specific so Docker behavior stays unchanged.
            format!("{}:{}:Z,U", host_path, container_path)
        } else {
            format!("{}:{}", host_path, container_path)
        }
    }

    async fn is_podman_engine(&self) -> bool {
        let Ok(version) = self.docker.version().await else {
            return false;
        };

        if version
            .version
            .as_deref()
            .map(|v| v.to_ascii_lowercase().contains("podman"))
            .unwrap_or(false)
        {
            return true;
        }

        version
            .components
            .unwrap_or_default()
            .iter()
            .any(|component| component.name.to_ascii_lowercase().contains("podman"))
    }

    fn is_container_running(info: &bollard::models::ContainerInspectResponse) -> bool {
        if info.state.as_ref().and_then(|s| s.running).unwrap_or(false) {
            return true;
        }

        info.state
            .as_ref()
            .and_then(|s| s.status.as_ref())
            .map(|s| format!("{s:?}").to_ascii_lowercase().contains("running"))
            .unwrap_or(false)
    }
}

// ---------------------------------------------------------------------------
// Compute implementation
// ---------------------------------------------------------------------------

#[async_trait]
impl Compute for DockerCompute {
    #[instrument(skip(self, definition))]
    async fn provision(&self, definition: &ComputeDefinition) -> Result<InstanceId> {
        use std::collections::HashMap;

        // Ensure the image exists locally by pulling it if necessary.
        let pull_opts = bollard::query_parameters::CreateImageOptionsBuilder::default()
            .from_image(definition.image.as_str())
            .build();
        self.docker
            .create_image(Some(pull_opts), None, None)
            .try_collect::<Vec<_>>()
            .await
            .map_err(|e| classify(definition.image.as_str(), e))?;

        let prefix = if definition.image.to_ascii_lowercase().contains("mysql") {
            "gfs-mysql"
        } else {
            "gfs-postgres"
        };
        let name = format!(
            "{}-{}",
            prefix,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis()
        );
        let env: Vec<String> = definition
            .env
            .iter()
            .map(|e| {
                let value = e.default.as_deref().unwrap_or("");
                format!("{}={}", e.name, value)
            })
            .collect();

        let mut port_bindings = HashMap::new();
        for p in &definition.ports {
            let key = format!("{}/tcp", p.compute_port);
            let host_port = p
                .host_port
                .map(|port| port.to_string())
                .unwrap_or_else(|| "0".to_string());
            let binding = bollard::service::PortBinding {
                host_ip: Some(String::new()),
                host_port: Some(host_port),
            };
            port_bindings.insert(key, Some(vec![binding]));
        }

        let mut binds = Vec::new();
        if let Some(ref host_data) = definition.host_data_dir {
            let host_path = host_data.to_string_lossy();
            let container_path = definition.data_dir.to_string_lossy();
            binds.push(self.bind_mount_spec(&host_path, &container_path).await);
        }

        let host_config = bollard::service::HostConfig {
            binds: if binds.is_empty() { None } else { Some(binds) },
            port_bindings: Some(port_bindings),
            ..Default::default()
        };

        let config = bollard::models::ContainerCreateBody {
            image: Some(definition.image.clone()),
            env: Some(env),
            exposed_ports: Some(
                definition
                    .ports
                    .iter()
                    .map(|p| format!("{}/tcp", p.compute_port))
                    .collect(),
            ),
            host_config: Some(host_config),
            user: definition.user.clone(),
            cmd: if definition.args.is_empty() {
                None
            } else {
                Some(definition.args.clone())
            },
            ..Default::default()
        };

        let options = bollard::query_parameters::CreateContainerOptionsBuilder::default()
            .name(&name)
            .build();

        let _create = self
            .docker
            .create_container(Some(options), config)
            .await
            .map_err(|e| classify(&name, e))?;

        // Store the container name in config (not the ID) so that after restart/recreate
        // the same name can be used to look up the container; Docker assigns a new ID on recreate.
        Ok(InstanceId(name))
    }

    #[instrument(skip(self, _options))]
    async fn start(&self, id: &InstanceId, _options: StartOptions) -> Result<InstanceStatus> {
        self.docker
            .start_container(
                &id.0,
                None::<bollard::query_parameters::StartContainerOptions>,
            )
            .await
            .map_err(|e| classify(&id.0, e))?;
        self.status(id).await
    }

    #[instrument(skip(self))]
    async fn stop(&self, id: &InstanceId) -> Result<InstanceStatus> {
        self.docker
            .stop_container(&id.0, None)
            .await
            .map_err(|e| classify(&id.0, e))?;
        self.wait_until_not_running(id).await?;
        self.status(id).await
    }

    #[instrument(skip(self))]
    async fn restart(&self, id: &InstanceId) -> Result<InstanceStatus> {
        self.docker
            .restart_container(&id.0, None)
            .await
            .map_err(|e| classify(&id.0, e))?;
        self.status(id).await
    }

    #[instrument(skip(self))]
    async fn status(&self, id: &InstanceId) -> Result<InstanceStatus> {
        let info = self
            .docker
            .inspect_container(&id.0, None)
            .await
            .map_err(|e| classify(&id.0, e))?;

        let state = info.state.as_ref();

        let docker_state_str = state
            .and_then(|s| s.status.as_ref())
            .map(|s| format!("{s:?}").to_ascii_lowercase());

        let instance_state = match docker_state_str.as_deref() {
            Some(s) if s.contains("running") => InstanceState::Running,
            Some(s) if s.contains("paused") => InstanceState::Paused,
            Some(s) if s.contains("restarting") => InstanceState::Restarting,
            Some(s) if s.contains("exited") || s.contains("stopped") => InstanceState::Stopped,
            Some(s) if s.contains("dead") => InstanceState::Failed,
            Some(s) if s.contains("created") => InstanceState::Starting,
            Some(s) if s.contains("removing") => InstanceState::Stopping,
            _ => InstanceState::Unknown,
        };

        let pid = state
            .and_then(|s| s.pid)
            .and_then(|p| if p > 0 { Some(p as u32) } else { None });

        let started_at = state
            .and_then(|s| s.started_at.as_deref())
            .and_then(|ts| chrono::DateTime::parse_from_rfc3339(ts).ok())
            .map(|dt| dt.with_timezone(&chrono::Utc));

        let exit_code = state.and_then(|s| s.exit_code).map(|c| c as i32);

        // Return the actual container ID from Docker so the UI shows the current ID
        // (config stores the container name for stable lookup across restarts).
        let display_id = info.id.as_deref().unwrap_or(&id.0).to_string();

        Ok(InstanceStatus {
            id: InstanceId(display_id),
            state: instance_state,
            pid,
            started_at,
            exit_code,
        })
    }

    #[instrument(skip(self, id, compute_port))]
    async fn get_connection_info(
        &self,
        id: &InstanceId,
        compute_port: u16,
    ) -> Result<InstanceConnectionInfo> {
        let info = self
            .docker
            .inspect_container(&id.0, None)
            .await
            .map_err(|e| classify(&id.0, e))?;

        let port_key = format!("{}/tcp", compute_port);
        let host_port = info
            .network_settings
            .as_ref()
            .and_then(|n| n.ports.as_ref())
            .and_then(|p| p.get(&port_key))
            .and_then(|b| b.as_ref())
            .and_then(|v| v.first())
            .and_then(|b| b.host_port.as_ref())
            .filter(|s| !s.is_empty())
            .and_then(|s| s.parse::<u16>().ok())
            .unwrap_or(compute_port);

        let env = info
            .config
            .as_ref()
            .and_then(|c| c.env.as_ref())
            .map(|e| {
                e.iter()
                    .filter_map(|s| {
                        let (k, v) = s.split_once('=')?;
                        Some((k.to_string(), v.to_string()))
                    })
                    .collect()
            })
            .unwrap_or_default();

        Ok(InstanceConnectionInfo {
            host: "localhost".to_string(),
            port: host_port,
            env,
        })
    }

    #[instrument(skip(self, commands))]
    async fn prepare_for_snapshot(&self, id: &InstanceId, commands: &[String]) -> Result<()> {
        for cmd in commands {
            if cmd.trim().is_empty() {
                continue;
            }
            self.run_exec_command(id, cmd).await?;
        }

        if self.is_podman_engine().await
            && let Some(data_mount) = self.get_instance_data_mount_container_path(id).await?
        {
            // Rootless Podman can map container-owned data files to subordinate UIDs
            // on the host. Ensure they are host-readable before filesystem snapshot.
            let escaped = data_mount.replace('\'', "'\"'\"'");
            let chmod_cmd = format!("chmod -R a+rX '{}'", escaped);
            self.run_exec_command(id, &chmod_cmd).await?;
        }

        Ok(())
    }

    #[instrument(skip(self))]
    async fn logs(&self, id: &InstanceId, options: LogsOptions) -> Result<Vec<LogEntry>> {
        let since_secs = options.since.map(|dt| dt.timestamp() as i32).unwrap_or(0);

        let tail = options
            .tail
            .map(|n| n.to_string())
            .unwrap_or_else(|| "all".to_string());

        let bollard_opts = bollard::query_parameters::LogsOptionsBuilder::default()
            .stdout(options.stdout)
            .stderr(options.stderr)
            .since(since_secs)
            .tail(&tail)
            .timestamps(true)
            .build();

        let mut stream = self.docker.logs(&id.0, Some(bollard_opts));
        let mut entries = Vec::new();

        while let Some(item) = stream.next().await {
            let output = item.map_err(|e| classify(&id.0, e))?;

            let (stream_type, bytes) = match output {
                bollard::container::LogOutput::StdOut { message } => (LogStream::Stdout, message),
                bollard::container::LogOutput::StdErr { message } => (LogStream::Stderr, message),
                bollard::container::LogOutput::Console { message } => (LogStream::Stdout, message),
                bollard::container::LogOutput::StdIn { .. } => continue,
            };

            let raw = String::from_utf8_lossy(&bytes);

            // Docker prepends an RFC 3339 timestamp followed by a space when
            // `timestamps=true` is set. Split it off to populate LogEntry.
            let parts: Vec<&str> = raw.splitn(2, ' ').collect();
            let (timestamp, message) = if parts.len() == 2 {
                let ts = chrono::DateTime::parse_from_rfc3339(parts[0])
                    .map(|dt| dt.with_timezone(&chrono::Utc))
                    .unwrap_or_else(|_| chrono::Utc::now());
                (ts, parts[1].to_owned())
            } else {
                (chrono::Utc::now(), raw.into_owned())
            };

            entries.push(LogEntry {
                timestamp,
                stream: stream_type,
                message,
            });
        }

        Ok(entries)
    }

    #[instrument(skip(self))]
    async fn pause(&self, id: &InstanceId) -> Result<InstanceStatus> {
        self.docker
            .pause_container(&id.0)
            .await
            .map_err(|e| classify(&id.0, e))?;
        self.status(id).await
    }

    #[instrument(skip(self))]
    async fn unpause(&self, id: &InstanceId) -> Result<InstanceStatus> {
        self.docker
            .unpause_container(&id.0)
            .await
            .map_err(|e| classify(&id.0, e))?;
        self.status(id).await
    }

    async fn get_instance_data_mount_host_path(
        &self,
        id: &InstanceId,
        compute_data_path: &str,
    ) -> Result<Option<std::path::PathBuf>> {
        let info = self
            .docker
            .inspect_container(&id.0, None)
            .await
            .map_err(|e| classify(&id.0, e))?;

        let binds = info
            .host_config
            .as_ref()
            .and_then(|h| h.binds.as_ref())
            .into_iter()
            .flatten();

        let container_path_normalized = compute_data_path.trim_end_matches('/');
        for bind in binds {
            let parts: Vec<&str> = bind.splitn(3, ':').collect();
            if parts.len() >= 2 {
                let host = parts[0];
                let container = parts[1].trim_end_matches('/');
                if container == container_path_normalized {
                    return Ok(Some(std::path::PathBuf::from(host)));
                }
            }
        }
        Ok(None)
    }

    async fn remove_instance(&self, id: &InstanceId) -> Result<()> {
        let _ = self.docker.stop_container(&id.0, None).await;
        let _ = self.wait_until_not_running(id).await;
        self.docker
            .remove_container(&id.0, None)
            .await
            .map_err(|e| classify(&id.0, e))?;
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Task execution (sidecar / ephemeral instances)
    // -----------------------------------------------------------------------

    #[instrument(skip(self))]
    async fn get_task_connection_info(
        &self,
        id: &InstanceId,
        compute_port: u16,
    ) -> Result<InstanceConnectionInfo> {
        let info = self
            .docker
            .inspect_container(&id.0, None)
            .await
            .map_err(|e| classify(&id.0, e))?;

        if !Self::is_container_running(&info) {
            return Err(ComputeError::NotRunning(id.0.clone()));
        }

        // Get the container IP on its first available network.
        let ip = info
            .network_settings
            .as_ref()
            .and_then(|n| n.networks.as_ref())
            .and_then(|nets| nets.values().next())
            .and_then(|net| net.ip_address.as_ref())
            .filter(|ip| !ip.is_empty())
            .cloned()
            .ok_or_else(|| {
                ComputeError::Internal(format!(
                    "instance has no task-reachable network IP: '{}'",
                    id.0
                ))
            })?;

        // Reuse the env extraction logic from get_connection_info.
        let env = info
            .config
            .as_ref()
            .and_then(|c| c.env.as_ref())
            .map(|e| {
                e.iter()
                    .filter_map(|s| {
                        let (k, v) = s.split_once('=')?;
                        Some((k.to_string(), v.to_string()))
                    })
                    .collect()
            })
            .unwrap_or_default();

        Ok(InstanceConnectionInfo {
            host: ip,
            port: compute_port,
            env,
        })
    }

    #[instrument(skip(self, definition, command))]
    async fn run_task(
        &self,
        definition: &ComputeDefinition,
        command: &str,
        linked_to: Option<&InstanceId>,
    ) -> Result<ExecOutput> {
        // 1. Pull image.
        let pull_opts = bollard::query_parameters::CreateImageOptionsBuilder::default()
            .from_image(definition.image.as_str())
            .build();
        self.docker
            .create_image(Some(pull_opts), None, None)
            .try_collect::<Vec<_>>()
            .await
            .map_err(|e| classify(definition.image.as_str(), e))?;

        // 2. Resolve the network of the linked instance so the task can reach it.
        let linked_network = if let Some(target) = linked_to {
            let info = self
                .docker
                .inspect_container(&target.0, None)
                .await
                .map_err(|e| classify(&target.0, e))?;
            if !Self::is_container_running(&info) {
                return Err(ComputeError::NotRunning(target.0.clone()));
            }

            let network = info
                .network_settings
                .as_ref()
                .and_then(|n| n.networks.as_ref())
                .and_then(|nets| nets.keys().next().cloned())
                .ok_or_else(|| {
                    ComputeError::Internal(format!(
                        "instance has no network for task linking: '{}'",
                        target.0
                    ))
                })?;

            Some(network)
        } else {
            None
        };

        // 3. Build environment variables.
        let env: Vec<String> = definition
            .env
            .iter()
            .map(|e| {
                let value = e.default.as_deref().unwrap_or("");
                format!("{}={}", e.name, value)
            })
            .collect();

        // 4. Bind mounts for data exchange.
        let mut binds = Vec::new();
        if let Some(ref host_data) = definition.host_data_dir {
            let host_path = host_data.to_string_lossy();
            let container_path = definition.data_dir.to_string_lossy();
            binds.push(self.bind_mount_spec(&host_path, &container_path).await);
        }

        let host_config = bollard::service::HostConfig {
            binds: if binds.is_empty() { None } else { Some(binds) },
            ..Default::default()
        };

        // 5. Create the task container with entrypoint overridden to sh -c.
        let task_name = format!(
            "gfs-task-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis()
        );

        let config = bollard::models::ContainerCreateBody {
            image: Some(definition.image.clone()),
            env: Some(env),
            host_config: Some(host_config),
            entrypoint: Some(vec!["sh".into(), "-c".into()]),
            cmd: Some(vec![command.to_string()]),
            ..Default::default()
        };

        let create_opts = bollard::query_parameters::CreateContainerOptionsBuilder::default()
            .name(&task_name)
            .build();

        self.docker
            .create_container(Some(create_opts), config)
            .await
            .map_err(|e| classify(&task_name, e))?;

        // 6. Connect to the linked instance's network if needed.
        if let Some(network) = &linked_network {
            let connect_opts = bollard::models::NetworkConnectRequest {
                container: task_name.clone(),
                ..Default::default()
            };
            self.docker
                .connect_network(network, connect_opts)
                .await
                .map_err(|e| classify(&task_name, e))?;
        }

        // 7. Start the container, wait for exit, capture output, clean up.
        //    On any failure after start, we still try to remove the container.
        let result = async {
            self.docker
                .start_container(
                    &task_name,
                    None::<bollard::query_parameters::StartContainerOptions>,
                )
                .await
                .map_err(|e| classify(&task_name, e))?;

            // Wait for the container to finish.
            let task_id = InstanceId(task_name.clone());
            self.wait_until_not_running(&task_id).await?;

            // Capture stdout/stderr from logs.
            let log_opts = bollard::query_parameters::LogsOptionsBuilder::default()
                .stdout(true)
                .stderr(true)
                .build();

            let mut stream = self.docker.logs(&task_name, Some(log_opts));
            let mut stdout = String::new();
            let mut stderr = String::new();

            while let Some(item) = stream.next().await {
                let output = item.map_err(|e| classify(&task_name, e))?;
                match output {
                    bollard::container::LogOutput::StdOut { message } => {
                        stdout.push_str(&String::from_utf8_lossy(&message));
                    }
                    bollard::container::LogOutput::StdErr { message } => {
                        stderr.push_str(&String::from_utf8_lossy(&message));
                    }
                    _ => {}
                }
            }

            // Get exit code.
            let info = self
                .docker
                .inspect_container(&task_name, None)
                .await
                .map_err(|e| classify(&task_name, e))?;

            let exit_code = info.state.as_ref().and_then(|s| s.exit_code).unwrap_or(-1) as i32;

            Ok(ExecOutput {
                exit_code,
                stdout,
                stderr,
            })
        }
        .await;

        // 8. Always remove the task container (best effort).
        let _ = self.docker.remove_container(&task_name, None).await;

        result
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

impl DockerCompute {
    async fn run_exec_command(&self, id: &InstanceId, cmd: &str) -> Result<()> {
        let opts = bollard::exec::CreateExecOptions {
            cmd: Some(vec!["sh".into(), "-c".into(), cmd.to_string()]),
            attach_stdout: Some(true),
            attach_stderr: Some(true),
            ..Default::default()
        };
        let exec = self
            .docker
            .create_exec(&id.0, opts)
            .await
            .map_err(|e| classify(&id.0, e))?;

        match self
            .docker
            .start_exec(&exec.id, None::<bollard::exec::StartExecOptions>)
            .await
            .map_err(|e| classify(&id.0, e))?
        {
            bollard::exec::StartExecResults::Attached { output, .. } => {
                output
                    .try_collect::<Vec<_>>()
                    .await
                    .map_err(|e| classify(&id.0, e))?;
            }
            bollard::exec::StartExecResults::Detached => {}
        }

        let inspect = self
            .docker
            .inspect_exec(&exec.id)
            .await
            .map_err(|e| classify(&id.0, e))?;

        if inspect.exit_code != Some(0) {
            return Err(gfs_domain::ports::compute::ComputeError::Internal(format!(
                "prepare_for_snapshot command failed (exit {:?}): {}",
                inspect.exit_code, cmd
            )));
        }

        Ok(())
    }

    async fn get_instance_data_mount_container_path(
        &self,
        id: &InstanceId,
    ) -> Result<Option<String>> {
        let info = self
            .docker
            .inspect_container(&id.0, None)
            .await
            .map_err(|e| classify(&id.0, e))?;

        let binds = info
            .host_config
            .as_ref()
            .and_then(|h| h.binds.as_ref())
            .into_iter()
            .flatten();

        for bind in binds {
            let parts: Vec<&str> = bind.splitn(3, ':').collect();
            if parts.len() >= 2 {
                return Ok(Some(parts[1].trim_end_matches('/').to_string()));
            }
        }

        Ok(None)
    }

    /// Wait until the container has reached a not-running state (e.g. exited).
    /// Use after stop_container so the snapshot or remove happens only once the container is fully stopped.
    async fn wait_until_not_running(&self, id: &InstanceId) -> Result<()> {
        let wait_opts = bollard::query_parameters::WaitContainerOptionsBuilder::default()
            .condition("not-running")
            .build();
        let mut stream = self.docker.wait_container(&id.0, Some(wait_opts));
        if let Some(item) = stream.next().await
            && let Err(e) = item
        {
            use bollard::errors::Error as BollardError;
            // DockerContainerWaitError means the container exited (possibly with non-zero code).
            // Treat as success so run_task can capture logs and exit code for the caller.
            if matches!(e, BollardError::DockerContainerWaitError { .. }) {
                return Ok(());
            }
            let msg = format!("Docker container wait error: {e:?}");
            return Err(ComputeError::Internal(msg));
        }
        Ok(())
    }
}
