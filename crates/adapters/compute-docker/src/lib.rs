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
//! * On Windows, host bind paths are normalized (extended `\\?\` prefixes are
//!   stripped) so Docker’s bind parser does not mis-handle `host:container` specs.

pub mod containers;
mod error;

use std::io::ErrorKind;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use futures_util::{StreamExt, TryStreamExt};
use gfs_domain::ports::compute::{
    Compute, ComputeCapabilities, ComputeDefinition, ComputeError, ExecOutput,
    InstanceConnectionInfo, InstanceId, InstanceState, InstanceStatus, LogEntry, LogStream,
    LogsOptions, Result, RuntimeDescriptor, StartOptions,
};
use tracing::instrument;

use crate::error::{classify, classify_with_mount_path};

/// Reject tar paths that could escape `dest` (`..`, absolute components, etc.).
fn tar_stripped_path_is_safe(stripped: &Path) -> bool {
    if stripped.as_os_str().is_empty() {
        return false;
    }
    for c in stripped.components() {
        match c {
            std::path::Component::Normal(_) | std::path::Component::CurDir => {}
            std::path::Component::ParentDir => return false,
            std::path::Component::RootDir | std::path::Component::Prefix(_) => return false,
        }
    }
    true
}

/// Validate a symlink target: must be non-empty, relative, and contain no `..` or root components.
fn tar_link_target_is_safe(target: &Path) -> bool {
    !target.as_os_str().is_empty()
        && !target.is_absolute()
        && target.components().all(|c| {
            matches!(
                c,
                std::path::Component::Normal(_) | std::path::Component::CurDir
            )
        })
}

/// Host path string suitable for Docker bind mounts. Verbatim Windows paths
/// (`\\?\...`) break Docker’s `host:container` parsing; map them to normal paths.
fn host_path_for_docker_bind(path: &Path) -> String {
    #[cfg(windows)]
    {
        let s = path.to_string_lossy();
        let s = s.as_ref();
        if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
            return format!(r"\\{}", rest);
        }
        if let Some(rest) = s.strip_prefix(r"\\?\") {
            return rest.to_string();
        }
        s.to_string()
    }
    #[cfg(not(windows))]
    {
        path.to_string_lossy().into_owned()
    }
}

fn resolve_host_bind_path(path: &Path) -> Result<std::path::PathBuf> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(ComputeError::Io)?
            .join(path)
    };

    Ok(absolute.canonicalize().unwrap_or(absolute))
}

// ---------------------------------------------------------------------------
// DockerCompute
// ---------------------------------------------------------------------------

/// Compute adapter backed by a local Docker daemon via [`bollard`].
#[derive(Debug, Clone)]
pub struct DockerCompute {
    docker: bollard::Docker,
    /// Optional platform override (e.g. `"linux/amd64"`) for pulling/creating
    /// containers — lets an amd64-only image run on arm64 under emulation.
    platform: Option<String>,
}

impl DockerCompute {
    /// Set a platform override (e.g. `"linux/amd64"`) applied when pulling and
    /// creating containers. Useful for images lacking a native-arch manifest.
    pub fn with_platform(mut self, platform: Option<String>) -> Self {
        self.platform = platform;
        self
    }

    /// Connect to the local Docker daemon using platform defaults
    /// (`/var/run/docker.sock` on Unix, named pipe on Windows).
    ///
    /// Returns an error if the socket cannot be opened (e.g. Docker is not
    /// running or the socket path has wrong permissions).
    pub fn new() -> std::result::Result<Self, ComputeError> {
        match bollard::Docker::connect_with_local_defaults() {
            Ok(docker) => Ok(Self { docker, platform: None }),
            Err(default_err) => {
                #[cfg(unix)]
                if let Some(socket_path) = Self::podman_socket_path() {
                    let socket = socket_path.to_string_lossy();

                    if let Ok(docker) = bollard::Docker::connect_with_unix(
                        socket.as_ref(),
                        120,
                        bollard::API_DEFAULT_VERSION,
                    ) {
                        return Ok(Self { docker, platform: None });
                    }

                    let socket_uri = format!("unix://{}", socket);
                    if let Ok(docker) = bollard::Docker::connect_with_unix(
                        &socket_uri,
                        120,
                        bollard::API_DEFAULT_VERSION,
                    ) {
                        return Ok(Self { docker, platform: None });
                    }
                }

                tracing::debug!("Docker connect error: {default_err}");
                Err(ComputeError::NotAvailable(Self::format_connection_error(
                    &default_err,
                )))
            }
        }
    }

    /// Convert a bollard connection error into a user-friendly error message.
    /// Detects common connection failure scenarios and provides actionable hints.
    pub fn format_connection_error(err: &bollard::errors::Error) -> String {
        let err_str = err.to_string();
        let err_lower = err_str.to_ascii_lowercase();

        let is_connection_error = err_lower.contains("connect")
            || err_lower.contains("connection refused")
            || err_lower.contains("connection reset")
            || err_lower.contains("no such file")
            || err_lower.contains("socket not found")
            || err_lower.contains("permission denied")
            || err_lower.contains("hyper legacy client");

        if !is_connection_error {
            return format!("Docker connection error: {}", err_str);
        }

        let is_permission_error =
            err_lower.contains("permission denied") || err_lower.contains("access denied");

        let is_socket_missing = err_lower.contains("no such file")
            || err_lower.contains("socket not found")
            || err_lower.contains("cannot connect");

        #[cfg(unix)]
        let hints = if is_permission_error {
            vec![
                "Docker/Podman daemon is running but current user lacks permissions",
                "Add your user to the docker group: sudo usermod -aG docker $USER",
                "Or run with sudo (not recommended for security)",
                "For Podman rootless: ensure podman socket is accessible",
            ]
        } else if is_socket_missing {
            vec![
                "Docker/Podman daemon is not running",
                "Start Docker: sudo systemctl start docker (or start Docker Desktop)",
                "Start Podman: systemctl --user start podman.socket (for rootless)",
                "Verify: docker ps or podman ps",
            ]
        } else {
            vec![
                "Docker/Podman daemon is not accessible",
                "Check if Docker/Podman service is running",
                "Verify socket permissions: ls -l /var/run/docker.sock",
                "For Podman rootless: check XDG_RUNTIME_DIR/podman/podman.sock",
            ]
        };

        #[cfg(windows)]
        let hints = vec![
            "Docker Desktop is not running",
            "Start Docker Desktop from the Start menu",
            "Verify Docker is running: docker ps",
        ];

        format!(
            "GFS was not able to connect to Docker/Podman.\n\n\
            Check the following:\n{}\n\n\
            Original error: {}",
            hints
                .iter()
                .map(|h| format!("- {}", h))
                .collect::<Vec<_>>()
                .join("\n"),
            err_str
        )
    }

    #[cfg(unix)]
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

    async fn describe_runtime_impl(&self) -> Result<RuntimeDescriptor> {
        let version = self.docker.version().await.map_err(|e| classify("", e))?;

        let is_podman = version
            .version
            .as_deref()
            .map(|v| v.to_ascii_lowercase().contains("podman"))
            .unwrap_or(false)
            || version
                .components
                .as_ref()
                .into_iter()
                .flat_map(|components| components.iter())
                .any(|component| component.name.to_ascii_lowercase().contains("podman"));

        Ok(RuntimeDescriptor {
            provider: if is_podman { "podman" } else { "docker" }.to_string(),
            version: version.version.unwrap_or_else(|| {
                if is_podman {
                    "unknown".to_string()
                } else {
                    "24".to_string()
                }
            }),
        })
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

    async fn wait_for_stable_start(&self, id: &InstanceId) -> Result<InstanceStatus> {
        let mut last = self.status(id).await?;
        for _ in 0..5 {
            match last.state {
                InstanceState::Stopped | InstanceState::Failed => {
                    return Err(ComputeError::Internal(format!(
                        "container '{}' exited during startup (exit {:?})",
                        id.0, last.exit_code
                    )));
                }
                _ => {}
            }

            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            last = self.status(id).await?;
        }

        match last.state {
            InstanceState::Stopped | InstanceState::Failed => Err(ComputeError::Internal(format!(
                "container '{}' exited during startup (exit {:?})",
                id.0, last.exit_code
            ))),
            _ => {
                self.wait_for_ready_ports(id).await;
                Ok(last)
            }
        }
    }

    async fn wait_for_ready_ports(&self, id: &InstanceId) {
        let Ok(info) = self.docker.inspect_container(&id.0, None).await else {
            return;
        };

        let host_ports: Vec<u16> = info
            .network_settings
            .as_ref()
            .and_then(|settings| settings.ports.as_ref())
            .into_iter()
            .flat_map(|ports| ports.values())
            .flatten()
            .flatten()
            .filter_map(|binding| binding.host_port.as_deref())
            .filter_map(|port| port.parse::<u16>().ok())
            .collect();

        if host_ports.is_empty() {
            return;
        }

        for _ in 0..40 {
            let mut all_ready = true;

            for port in &host_ports {
                let ipv4_ready = tokio::net::TcpStream::connect(("127.0.0.1", *port))
                    .await
                    .is_ok();
                let ipv6_ready = tokio::net::TcpStream::connect(("::1", *port)).await.is_ok();

                if !ipv4_ready && !ipv6_ready {
                    all_ready = false;
                    break;
                }
            }

            if all_ready {
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                return;
            }

            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        }
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

        // Ensure the image exists locally, pulling it if necessary. A pull failure
        // is TOLERATED when the image is already present locally — e.g. a
        // locally-built image (like `gfs-postgres:16`) that was never pushed to a
        // registry, so `docker pull` 404s but `docker run` would work.
        let mut pull_builder = bollard::query_parameters::CreateImageOptionsBuilder::default()
            .from_image(definition.image.as_str());
        if let Some(p) = self.platform.as_deref() {
            pull_builder = pull_builder.platform(p);
        }
        let pull_opts = pull_builder.build();
        if let Err(e) = self
            .docker
            .create_image(Some(pull_opts), None, None)
            .try_collect::<Vec<_>>()
            .await
        {
            // Fall back to a locally-present image; only surface the pull error
            // (missing tag, no manifest for this arch, auth) if it isn't local.
            if self.docker.inspect_image(&definition.image).await.is_err() {
                return Err(ComputeError::Internal(format!(
                    "failed to pull image '{}': {e}{}",
                    definition.image,
                    if self.platform.is_none() {
                        " (if the image lacks a manifest for your architecture, retry with --platform linux/amd64)"
                    } else {
                        ""
                    }
                )));
            }
            tracing::debug!(
                image = %definition.image,
                error = %e,
                "image pull failed but the image is present locally; using the local image"
            );
        }

        let image_name = definition.image.to_ascii_lowercase();
        let prefix = if image_name.contains("mysql") {
            "gfs-mysql"
        } else if image_name.contains("clickhouse") {
            "gfs-clickhouse"
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
            let host_path = host_path_for_docker_bind(&resolve_host_bind_path(host_data)?);
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
            labels: if definition.labels.is_empty() {
                None
            } else {
                Some(definition.labels.clone().into_iter().collect())
            },
            ..Default::default()
        };

        let mut create_builder =
            bollard::query_parameters::CreateContainerOptionsBuilder::default().name(&name);
        if let Some(p) = self.platform.as_deref() {
            create_builder = create_builder.platform(p);
        }
        let options = create_builder.build();

        let mount_path = definition.host_data_dir.clone();
        let _create = self
            .docker
            .create_container(Some(options), config)
            .await
            .map_err(|e| classify_with_mount_path(&name, e, mount_path))?;

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
        self.wait_for_stable_start(id).await
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
        self.wait_for_stable_start(id).await
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
            let out = self.run_exec_command(id, cmd, Some("0:0")).await?;
            if out.exit_code != 0 {
                return Err(ComputeError::Internal(format!(
                    "prepare_for_snapshot command failed (exit {}): {}\nstderr: {}",
                    out.exit_code,
                    cmd,
                    out.stderr.trim()
                )));
            }
        }

        if self.is_podman_engine().await
            && let Some(data_mount) = self.get_instance_data_mount_container_path(id).await?
        {
            // Rootless Podman can map container-owned data files to subordinate UIDs
            // on the host. Ensure they are host-readable before filesystem snapshot.
            let escaped = data_mount.replace('\'', "'\"'\"'");
            let chmod_cmd = format!("chmod -R a+rX '{}'", escaped);
            let out = self.run_exec_command(id, &chmod_cmd, Some("0:0")).await?;
            if out.exit_code != 0 {
                return Err(ComputeError::Internal(format!(
                    "prepare_for_snapshot chmod failed (exit {}): {}\nstderr: {}",
                    out.exit_code,
                    chmod_cmd,
                    out.stderr.trim()
                )));
            }
        }

        Ok(())
    }

    async fn describe_runtime(&self) -> Result<RuntimeDescriptor> {
        self.describe_runtime_impl().await
    }

    async fn capabilities(&self) -> Result<ComputeCapabilities> {
        Ok(ComputeCapabilities {
            supports_stream_snapshot: true,
            supports_exec_as_root: true,
        })
    }

    async fn exec(&self, id: &InstanceId, command: &str, user: Option<&str>) -> Result<ExecOutput> {
        self.run_exec_command(id, command, user).await
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
        // 1. Pull image (tolerate a pull failure when it's already present locally,
        //    e.g. a locally-built image never pushed to a registry).
        let pull_opts = bollard::query_parameters::CreateImageOptionsBuilder::default()
            .from_image(definition.image.as_str())
            .build();
        if let Err(e) = self
            .docker
            .create_image(Some(pull_opts), None, None)
            .try_collect::<Vec<_>>()
            .await
        {
            if self.docker.inspect_image(&definition.image).await.is_err() {
                return Err(classify(definition.image.as_str(), e));
            }
        }

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
            let host_path = host_path_for_docker_bind(&resolve_host_bind_path(host_data)?);
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
            // Honour the user override from the definition (e.g. "0:0" for root-level repair tasks).
            user: definition.user.clone(),
            ..Default::default()
        };

        let create_opts = bollard::query_parameters::CreateContainerOptionsBuilder::default()
            .name(&task_name)
            .build();

        let mount_path = definition.host_data_dir.clone();
        self.docker
            .create_container(Some(create_opts), config)
            .await
            .map_err(|e| classify_with_mount_path(&task_name, e, mount_path))?;

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

    /// Stream a container's data directory out through `docker cp` and extract
    /// it into `dest`. Used as the permission-denied fallback when the host
    /// user cannot read files inside a bind-mounted container data dir.
    ///
    /// # Equivalence vs host-path snapshots
    ///
    /// On restore, a snapshot produced by this function must be
    /// indistinguishable from one produced by `storage.snapshot` (the host-path
    /// `cp --reflink=auto -a`). Three properties are worth spelling out:
    ///
    /// * **Directory modes.** This function chmods created directories to `0755`
    ///   during extraction; the host path preserves whatever mode cp `-a` copied.
    ///   Both converge because `FileStorage::snapshot` / `finalize_snapshot`
    ///   runs `chmod -R u+rX,u-w,go-rwx` on the final tree, normalizing every
    ///   directory to `0500` regardless of the starting mode.
    /// * **Ownership.** This function extracts files as the host user running
    ///   gfs. The host path preserves the container UID (e.g. `postgres:999`).
    ///   The two diverge on disk in the snapshot, but `pre_start_repair_data_dir`
    ///   in the checkout use case chowns the restored workspace to the target
    ///   UID on every container start, so restored DB state is equivalent.
    /// * **Hard-link topology.** Both paths preserve hard links (`cp -a` honors
    ///   them; tar archives encode hard-link entries, which we recreate via
    ///   `std::fs::hard_link`). Divergence can only occur if the hard-link
    ///   target hasn't been extracted yet (treated as a hard error — see the
    ///   `InvalidData` branch below) or if `hard_link` fails with EXDEV — which
    ///   is effectively unreachable because `src` and `dest_path` both live
    ///   under `dest`. A `warn!` fires if the EXDEV branch ever runs.
    ///
    /// Mode/permission clamps on individual file entries are intentionally
    /// suppressed via `set_preserve_permissions(false)` because tar headers
    /// from Docker may contain UID-specific modes the host user cannot stat;
    /// the final mode is established by `finalize_snapshot`.
    #[instrument(skip(self))]
    async fn stream_snapshot(
        &self,
        id: &InstanceId,
        container_path: &str,
        dest: &Path,
    ) -> Result<()> {
        let opts = bollard::query_parameters::DownloadFromContainerOptionsBuilder::default()
            .path(container_path)
            .build();

        let dest = dest.to_path_buf();

        // std::io::pipe() gives a synchronous (reader, writer) pair backed by
        // an OS pipe — no heap allocation of the full archive.
        // The writer end is fed from the async bollard stream; the reader end
        // is consumed by `tar::Archive` inside a blocking thread.  Both sides
        // run concurrently so back-pressure is handled by the OS pipe buffer.
        let (pipe_reader, mut pipe_writer) = std::io::pipe().map_err(ComputeError::Io)?;

        // Spawn the blocking unpack side first so the reader is already
        // consuming when the writer starts filling the pipe.
        let unpack = tokio::task::spawn_blocking(move || -> std::io::Result<usize> {
            use std::io;
            #[cfg(unix)]
            use std::os::unix::fs::PermissionsExt;

            fn chmod_best_effort(path: &std::path::Path, mode: u32) {
                #[cfg(unix)]
                {
                    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode));
                }
                #[cfg(not(unix))]
                {
                    let _ = mode;
                    let _ = path;
                }
            }

            fn repair_tree_for_removal(path: &std::path::Path) {
                #[cfg(unix)]
                {
                    if let Ok(md) = std::fs::symlink_metadata(path) {
                        if md.is_dir() {
                            chmod_best_effort(path, 0o700);
                            if let Ok(rd) = std::fs::read_dir(path) {
                                for e in rd.flatten() {
                                    repair_tree_for_removal(&e.path());
                                }
                            }
                        } else {
                            chmod_best_effort(path, 0o600);
                        }
                    }
                }
                #[cfg(not(unix))]
                {
                    let _ = path;
                }
            }

            // If a partial snapshot dir exists (from a failed host-side `cp` or a prior
            // interrupted stream_snapshot run), repair any 000-mode dirs that would block
            // traversal, then remove the whole tree so the fresh tar stream unpacks cleanly.
            if dest.exists() {
                repair_tree_for_removal(&dest);
                let _ = std::fs::remove_dir_all(&dest);
            }
            std::fs::create_dir_all(&dest)?;
            chmod_best_effort(&dest, 0o755);

            let mut archive = tar::Archive::new(pipe_reader);
            archive.set_preserve_permissions(false);
            let mut files = 0usize;
            for entry in archive.entries()? {
                let mut entry = entry?;
                let ty = entry.header().entry_type();

                // Compute path components for all entry types.
                let raw_path = entry.path()?.into_owned();
                // Strip the leading directory component Docker always adds.
                let stripped: PathBuf = raw_path.components().skip(1).collect();
                if stripped.as_os_str().is_empty() {
                    continue;
                }
                if !tar_stripped_path_is_safe(&stripped) {
                    return Err(io::Error::new(
                        ErrorKind::InvalidData,
                        format!("refusing unsafe tar path: {}", stripped.display()),
                    ));
                }
                let dest_path = dest.join(&stripped);

                if ty.is_symlink() {
                    let raw_target = entry
                        .header()
                        .link_name()?
                        .ok_or_else(|| {
                            io::Error::new(
                                ErrorKind::InvalidData,
                                format!(
                                    "stream_snapshot: symlink '{}' has no link target",
                                    stripped.display()
                                ),
                            )
                        })?
                        .into_owned();

                    if !tar_link_target_is_safe(&raw_target) {
                        // Absolute or parent-escaping targets are legitimate in some DB setups
                        // (e.g. PostgreSQL external tablespaces: pg_tblspc/<oid> → /mnt/...).
                        // Capture the symlink as-is so the directory structure is preserved,
                        // but warn that the target itself is not included in this snapshot.
                        tracing::warn!(
                            symlink = %stripped.display(),
                            target = %raw_target.display(),
                            "stream_snapshot: symlink points outside container data dir; \
                             capturing dangling link (target is not snapshotted)"
                        );
                    }
                    if let Some(parent) = dest_path.parent() {
                        std::fs::create_dir_all(parent)?;
                        chmod_best_effort(parent, 0o755);
                    }
                    #[cfg(unix)]
                    {
                        std::os::unix::fs::symlink(&raw_target, &dest_path)?;
                        continue;
                    }
                    #[cfg(not(unix))]
                    return Err(io::Error::new(
                        ErrorKind::Unsupported,
                        "stream_snapshot: symlinks not supported on this platform",
                    ));
                }

                if ty.is_hard_link() {
                    let raw_link = entry
                        .header()
                        .link_name()?
                        .ok_or_else(|| {
                            io::Error::new(
                                ErrorKind::InvalidData,
                                format!(
                                    "stream_snapshot: hard link '{}' has no source",
                                    stripped.display()
                                ),
                            )
                        })?
                        .into_owned();
                    let link_stripped: PathBuf = raw_link.components().skip(1).collect();
                    if !tar_stripped_path_is_safe(&link_stripped) {
                        return Err(io::Error::new(
                            ErrorKind::InvalidData,
                            format!(
                                "stream_snapshot: hard link '{}' points to unsafe path",
                                stripped.display()
                            ),
                        ));
                    }
                    let src = dest.join(&link_stripped);
                    if !src.exists() {
                        return Err(io::Error::new(
                            ErrorKind::InvalidData,
                            format!(
                                "stream_snapshot: hard link '{}' references '{}' which has \
                                 not yet been extracted — tar stream is out of order or the \
                                 archive is incomplete. This can happen with certain Docker \
                                 versions that emit links before their source; retrying the \
                                 commit or upgrading Docker typically resolves it",
                                stripped.display(),
                                link_stripped.display()
                            ),
                        ));
                    }
                    if let Some(parent) = dest_path.parent() {
                        std::fs::create_dir_all(parent)?;
                        chmod_best_effort(parent, 0o755);
                    }
                    match std::fs::hard_link(&src, &dest_path) {
                        Ok(()) => {}
                        Err(e) if e.kind() != ErrorKind::PermissionDenied => {
                            // Cross-device link (EXDEV), unsupported filesystem, etc.:
                            // fall back to a regular copy so the snapshot still succeeds.
                            //
                            // This branch should be effectively unreachable: `src` and
                            // `dest_path` are both below `dest` (the snapshot root), so
                            // they share a filesystem. If this fires, the operator has
                            // pointed `.gfs/snapshots/` at a path that crosses a mount
                            // boundary — which breaks the hard-link topology guarantee
                            // vs the host-snapshot path. Surfacing at `warn!` makes the
                            // rare event observable in production logs.
                            tracing::warn!(
                                source = %src.display(),
                                dest = %dest_path.display(),
                                error = %e,
                                "stream_snapshot: hard_link failed — falling back to copy; \
                                 snapshot's hard-link topology will differ from the source \
                                 (possible if .gfs/snapshots/ spans a mount boundary)"
                            );
                            std::fs::copy(&src, &dest_path)?;
                        }
                        Err(e) => return Err(e),
                    }
                    files += 1;
                    continue;
                }

                // Positive allow-list: at this point we've already handled symlinks and
                // hard links. Anything remaining must be a directory or a regular file
                // (`is_file()` covers both `Regular` and `Continuous`). Block, char, fifo,
                // and any future exotic type would otherwise fall through to
                // `entry.unpack()` and could create device nodes on the host — a container
                // compromise vector. Reject them explicitly.
                if !ty.is_dir() && !ty.is_file() {
                    return Err(io::Error::new(
                        ErrorKind::InvalidData,
                        format!(
                            "stream_snapshot: refusing to extract unexpected tar entry \
                             type {:?} for '{}'; database workspaces must contain only \
                             regular files, directories, symlinks, and hard links",
                            ty,
                            stripped.display()
                        ),
                    ));
                }

                if ty.is_dir() {
                    std::fs::create_dir_all(&dest_path)?;
                    chmod_best_effort(&dest_path, 0o755);
                } else {
                    if let Some(parent) = dest_path.parent() {
                        std::fs::create_dir_all(parent)?;
                        chmod_best_effort(parent, 0o755);
                    }
                    entry.unpack(&dest_path)?;
                    files += 1;
                }
            }
            Ok(files)
        });

        // Drive the bollard stream into the pipe writer on the async side.
        let container_name = id.0.clone();
        let mut stream = self.docker.download_from_container(&id.0, Some(opts));
        let write_result: Result<()> = async {
            while let Some(chunk) = stream.next().await {
                let chunk = chunk.map_err(|e| classify(&container_name, e))?;
                pipe_writer.write_all(&chunk).map_err(ComputeError::Io)?;
            }
            Ok(())
        }
        .await;

        // Drop writer so the reader sees EOF and the unpack task can finish.
        drop(pipe_writer);

        let files = unpack
            .await
            .map_err(|e| ComputeError::Internal(format!("tar unpack task panicked: {e}")))?
            .map_err(ComputeError::Io)?;

        // Surface any stream error after the unpack has drained.
        // Also guard against an empty archive — zero files means Docker returned
        // an empty (or header-only) tar, which would commit an empty snapshot and
        // cause the database to reinitialize on the next checkout (data loss).
        if files == 0 {
            return Err(ComputeError::Internal(format!(
                "stream_snapshot: container '{container_path}' produced an empty archive \
                 (0 regular files extracted); refusing to commit empty snapshot",
                container_path = container_path,
            )));
        }
        //
        // When `tar::Archive` reaches the end-of-archive markers it may stop reading
        // even if the Docker stream still has trailing padding bytes. That closes the
        // read end of the pipe and can produce a BrokenPipe on the writer.
        // This is harmless as long as extraction completed successfully.
        if let Err(e) = write_result {
            match &e {
                ComputeError::Io(ioe) if ioe.kind() == std::io::ErrorKind::BrokenPipe => {}
                _ => return Err(e),
            }
        }

        tracing::info!(
            container = %id.0,
            container_path,
            files,
            "stream_snapshot: unpacked tar archive from container"
        );
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

impl DockerCompute {
    async fn run_exec_command(
        &self,
        id: &InstanceId,
        cmd: &str,
        user: Option<&str>,
    ) -> Result<ExecOutput> {
        const MAX_CAPTURE_BYTES: usize = 256 * 1024; // per-stream cap

        let opts = bollard::exec::CreateExecOptions {
            cmd: Some(vec!["sh".into(), "-c".into(), cmd.to_string()]),
            attach_stdout: Some(true),
            attach_stderr: Some(true),
            user: user.map(|u| u.to_string()),
            ..Default::default()
        };
        let exec = self
            .docker
            .create_exec(&id.0, opts)
            .await
            .map_err(|e| classify(&id.0, e))?;

        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut stdout_truncated = false;
        let mut stderr_truncated = false;
        match self
            .docker
            .start_exec(&exec.id, None::<bollard::exec::StartExecOptions>)
            .await
            .map_err(|e| classify(&id.0, e))?
        {
            bollard::exec::StartExecResults::Attached { output, .. } => {
                let mut output = output;
                while let Some(item) = output.next().await {
                    let f = item.map_err(|e| classify(&id.0, e))?;
                    match f {
                        bollard::container::LogOutput::StdOut { message }
                        | bollard::container::LogOutput::Console { message } => {
                            if stdout.len() < MAX_CAPTURE_BYTES {
                                let remaining = MAX_CAPTURE_BYTES - stdout.len();
                                stdout.extend_from_slice(&message[..message.len().min(remaining)]);
                            } else {
                                stdout_truncated = true;
                            }
                        }
                        bollard::container::LogOutput::StdErr { message } => {
                            if stderr.len() < MAX_CAPTURE_BYTES {
                                let remaining = MAX_CAPTURE_BYTES - stderr.len();
                                stderr.extend_from_slice(&message[..message.len().min(remaining)]);
                            } else {
                                stderr_truncated = true;
                            }
                        }
                        bollard::container::LogOutput::StdIn { .. } => {}
                    }
                }
            }
            bollard::exec::StartExecResults::Detached => {}
        }

        let inspect = self
            .docker
            .inspect_exec(&exec.id)
            .await
            .map_err(|e| classify(&id.0, e))?;

        let mut stdout_s = String::from_utf8_lossy(&stdout).into_owned();
        let mut stderr_s = String::from_utf8_lossy(&stderr).into_owned();
        if stdout_truncated {
            stdout_s.push_str("\n<stdout truncated>\n");
        }
        if stderr_truncated {
            stderr_s.push_str("\n<stderr truncated>\n");
        }

        Ok(ExecOutput {
            exit_code: inspect
                .exit_code
                .and_then(|c| i32::try_from(c).ok())
                .unwrap_or(-1),
            stdout: stdout_s,
            stderr: stderr_s,
        })
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

#[cfg(test)]
mod tar_safety_tests {
    use super::tar_link_target_is_safe;
    use std::path::Path;

    #[test]
    fn relative_target_is_safe() {
        assert!(tar_link_target_is_safe(Path::new("pg_wal")));
        assert!(tar_link_target_is_safe(Path::new("./sub/dir")));
    }

    #[test]
    fn empty_target_is_unsafe() {
        assert!(!tar_link_target_is_safe(Path::new("")));
    }

    #[test]
    fn absolute_target_is_unsafe() {
        assert!(!tar_link_target_is_safe(Path::new("/tmp/external-wal")));
    }

    #[test]
    fn parent_dir_component_is_unsafe() {
        assert!(!tar_link_target_is_safe(Path::new("../escape")));
        assert!(!tar_link_target_is_safe(Path::new("a/../../b")));
    }
}

#[cfg(test)]
mod host_path_tests {
    use super::{DockerCompute, host_path_for_docker_bind, resolve_host_bind_path};
    use std::path::Path;

    #[test]
    #[cfg(windows)]
    fn strips_verbatim_drive_prefix() {
        assert_eq!(
            host_path_for_docker_bind(Path::new(r"\\?\C:\Users\test")),
            r"C:\Users\test"
        );
    }

    #[test]
    #[cfg(windows)]
    fn strips_verbatim_unc_prefix() {
        assert_eq!(
            host_path_for_docker_bind(Path::new(r"\\?\UNC\server\share\dir")),
            r"\\server\share\dir"
        );
    }

    #[test]
    #[cfg(not(windows))]
    fn passthrough_non_windows() {
        assert_eq!(
            host_path_for_docker_bind(Path::new("/tmp/foo/bar")),
            "/tmp/foo/bar"
        );
    }

    #[test]
    #[cfg(not(windows))]
    fn resolves_relative_bind_paths_to_absolute() {
        let current = std::env::current_dir().expect("current dir");
        let resolved = resolve_host_bind_path(Path::new("target")).expect("resolve relative path");
        assert_eq!(resolved, current.join("target"));
    }

    #[test]
    fn format_connection_error_connection_refused() {
        let err = bollard::errors::Error::DockerResponseServerError {
            status_code: 500,
            message: "connection refused".to_string(),
        };
        let formatted = DockerCompute::format_connection_error(&err);
        assert!(formatted.contains("GFS was not able to connect to Docker/Podman"));
        assert!(formatted.contains("Check the following"));
        assert!(formatted.contains("connection refused"));
    }

    #[test]
    fn format_connection_error_permission_denied() {
        let err = bollard::errors::Error::DockerResponseServerError {
            status_code: 403,
            message: "permission denied".to_string(),
        };
        let formatted = DockerCompute::format_connection_error(&err);
        assert!(formatted.contains("GFS was not able to connect to Docker/Podman"));
        #[cfg(unix)]
        {
            assert!(formatted.contains("permission denied"));
            assert!(formatted.contains("docker group"));
        }
    }

    #[test]
    fn format_connection_error_socket_missing() {
        let err = bollard::errors::Error::DockerResponseServerError {
            status_code: 404,
            message: "no such file or directory".to_string(),
        };
        let formatted = DockerCompute::format_connection_error(&err);
        assert!(formatted.contains("GFS was not able to connect to Docker/Podman"));
        #[cfg(unix)]
        {
            assert!(formatted.contains("not running"));
            assert!(formatted.contains("systemctl"));
        }
    }

    #[test]
    fn format_connection_error_bollard_socket_not_found() {
        let err = bollard::errors::Error::DockerResponseServerError {
            status_code: 500,
            message: "Socket not found: /var/run/docker.sock".to_string(),
        };
        let formatted = DockerCompute::format_connection_error(&err);
        assert!(formatted.contains("GFS was not able to connect to Docker/Podman"));
        #[cfg(unix)]
        {
            assert!(formatted.contains("not running"));
            assert!(formatted.contains("systemctl"));
        }
    }

    #[test]
    fn format_connection_error_hyper_legacy_client() {
        let err = bollard::errors::Error::DockerResponseServerError {
            status_code: 500,
            message: "Error in the hyper legacy client: client error (Connect)".to_string(),
        };
        let formatted = DockerCompute::format_connection_error(&err);
        assert!(formatted.contains("GFS was not able to connect to Docker/Podman"));
        assert!(formatted.contains("hyper legacy client"));
    }

    #[test]
    fn format_connection_error_generic_error() {
        let err = bollard::errors::Error::DockerResponseServerError {
            status_code: 500,
            message: "some other error".to_string(),
        };
        let formatted = DockerCompute::format_connection_error(&err);
        assert!(formatted.contains("Docker connection error"));
        assert!(formatted.contains("some other error"));
    }

    #[test]
    fn format_connection_error_includes_original_error() {
        let err = bollard::errors::Error::DockerResponseServerError {
            status_code: 500,
            message: "connection refused".to_string(),
        };
        let formatted = DockerCompute::format_connection_error(&err);
        assert!(formatted.contains("Original error"));
        assert!(formatted.contains("connection refused"));
    }
}
