//! Kubernetes compute adapter for the [`Compute`] port.
//!
//! Manages database instances as Kubernetes StatefulSets backed by a standalone
//! PVC (OpenEBS by default). Each GFS repo gets its own StatefulSet, headless
//! Service, and PVC identified by a stable name derived at provision time.
//!
//! # Naming convention
//!
//! Given `InstanceId("gfs-postgres-1714200000000")`:
//! - StatefulSet → `gfs-postgres-1714200000000`
//! - Service     → `gfs-postgres-1714200000000`
//! - PVC         → `gfs-postgres-1714200000000-data`
//! - Pod         → `gfs-postgres-1714200000000-0`

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use async_trait::async_trait;
use gfs_domain::ports::compute::{
    Compute, ComputeCapabilities, ComputeDefinition, ComputeError, ExecOutput,
    InstanceConnectionInfo, InstanceId, InstanceState, InstanceStatus, LogEntry, LogStream,
    LogsOptions, Result, RuntimeDescriptor, StartOptions,
};
use k8s_openapi::api::apps::v1::{StatefulSet, StatefulSetSpec};
use k8s_openapi::api::core::v1::{
    Container, EnvVar as K8sEnvVar, PersistentVolumeClaim, PersistentVolumeClaimSpec,
    PersistentVolumeClaimVolumeSource, Pod, PodSpec, PodTemplateSpec, Service, ServicePort,
    ServiceSpec, Volume, VolumeMount, VolumeResourceRequirements,
};
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{LabelSelector, ObjectMeta};
use k8s_openapi::apimachinery::pkg::util::intstr::IntOrString;
use kube::{
    Api, Client,
    api::{AttachParams, DeleteParams, LogParams, Patch, PatchParams, PostParams},
};
use tokio::io::AsyncReadExt;
use tracing::instrument;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

const DEFAULT_NAMESPACE: &str = "default";
const MANAGER: &str = "gfs";
const DEFAULT_PVC_SIZE: &str = "10Gi";
const SCALE_TIMEOUT: Duration = Duration::from_secs(180);
const POLL_INTERVAL_MS: u64 = 500;
const TASK_CONTAINER_NAME: &str = "task";
const SIDECAR_CONTAINER_NAME: &str = "sidecar";
const BUSYBOX_SIDECAR_IMAGE: &str = "busybox:1.36";

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

/// Runtime configuration for the Kubernetes compute adapter.
///
/// Build with [`K8sComputeConfig::default()`] or override individual fields.
/// Values can also be driven from environment variables via [`K8sComputeConfig::from_env`].
#[derive(Debug, Clone)]
pub struct K8sComputeConfig {
    /// Kubernetes namespace for all managed objects.  Defaults to `"default"`.
    pub namespace: String,
    /// StorageClass name for user database PVCs (e.g. `"openebs-hostpath"`).
    /// When `None`, the cluster's default StorageClass is used.
    pub storage_class: Option<String>,
    /// Extra labels applied to every managed object (StatefulSet, Service, PVC, pod template).
    /// Use for identity labels such as `guepard.io/org_id`, `guepard.io/project_id`.
    pub extra_labels: BTreeMap<String, String>,
}

impl Default for K8sComputeConfig {
    fn default() -> Self {
        Self {
            namespace: DEFAULT_NAMESPACE.to_string(),
            storage_class: None,
            extra_labels: BTreeMap::new(),
        }
    }
}

impl K8sComputeConfig {
    /// Build config from environment variables:
    /// - `GFS_K8S_NAMESPACE`       → namespace (default: `"default"`)
    /// - `GFS_K8S_STORAGE_CLASS`   → storage class (default: cluster default)
    /// - `GFS_ORG_ID`              → added as `guepard.io/org_id` label
    pub fn from_env() -> Self {
        let namespace = std::env::var("GFS_K8S_NAMESPACE")
            .unwrap_or_else(|_| DEFAULT_NAMESPACE.to_string());
        let storage_class = std::env::var("GFS_K8S_STORAGE_CLASS").ok();
        let mut extra_labels = BTreeMap::new();
        if let Ok(org_id) = std::env::var("GFS_ORG_ID") {
            extra_labels.insert("guepard.io/org_id".to_string(), org_id);
        }
        Self {
            namespace,
            storage_class,
            extra_labels,
        }
    }
}

// ---------------------------------------------------------------------------
// Tar path safety helpers (mirrors compute-docker safety model)
// ---------------------------------------------------------------------------

fn tar_stripped_path_is_safe(stripped: &Path) -> bool {
    if stripped.as_os_str().is_empty() {
        return false;
    }
    for c in stripped.components() {
        match c {
            std::path::Component::Normal(_) | std::path::Component::CurDir => {}
            _ => return false,
        }
    }
    true
}

fn tar_link_target_is_safe(target: &Path) -> bool {
    !target.as_os_str().is_empty()
        && !target.is_absolute()
        && target
            .components()
            .all(|c| matches!(c, std::path::Component::Normal(_) | std::path::Component::CurDir))
}

// ---------------------------------------------------------------------------
// K8sCompute
// ---------------------------------------------------------------------------

/// Compute adapter backed by the Kubernetes API server.
pub struct K8sCompute {
    client: Client,
    namespace: String,
    storage_class: Option<String>,
    extra_labels: BTreeMap<String, String>,
}

impl K8sCompute {
    /// Connect to the cluster using default kubeconfig resolution and default config.
    pub async fn new() -> std::result::Result<Self, ComputeError> {
        Self::with_config(K8sComputeConfig::from_env()).await
    }

    /// Connect to the cluster with explicit configuration.
    pub async fn with_config(config: K8sComputeConfig) -> std::result::Result<Self, ComputeError> {
        let client = Client::try_default().await.map_err(|e| {
            ComputeError::NotAvailable(format!(
                "GFS was not able to connect to the Kubernetes cluster.\n\n\
                 Check that:\n\
                 - `kubectl cluster-info` works\n\
                 - KUBECONFIG is set (or ~/.kube/config exists)\n\
                 - An in-cluster service account is mounted (for pod-based usage)\n\n\
                 Original error: {e}"
            ))
        })?;
        Ok(Self {
            client,
            namespace: config.namespace,
            storage_class: config.storage_class,
            extra_labels: config.extra_labels,
        })
    }

    // -----------------------------------------------------------------------
    // Naming helpers
    // -----------------------------------------------------------------------

    fn pod_name(id: &InstanceId) -> String {
        format!("{}-0", id.0)
    }

    fn pvc_name(id: &InstanceId) -> String {
        format!("{}-data", id.0)
    }

    fn app_labels(&self, id: &InstanceId) -> BTreeMap<String, String> {
        let mut labels = BTreeMap::from([
            ("app".to_string(), id.0.clone()),
            ("managed-by".to_string(), MANAGER.to_string()),
        ]);
        labels.extend(self.extra_labels.clone());
        labels
    }

    fn generate_name(image: &str) -> String {
        let prefix = if image.contains("mysql") {
            "gfs-mysql"
        } else if image.contains("clickhouse") {
            "gfs-clickhouse"
        } else {
            "gfs-postgres"
        };
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        format!("{}-{}", prefix, ts)
    }

    // -----------------------------------------------------------------------
    // Error classifier
    // -----------------------------------------------------------------------

    fn classify_err(id: &str, err: kube::Error) -> ComputeError {
        match &err {
            kube::Error::Api(resp) if resp.code == 404 => ComputeError::NotFound(id.to_string()),
            _ => ComputeError::Internal(err.to_string()),
        }
    }

    // -----------------------------------------------------------------------
    // Timestamp conversion (jiff → chrono)
    // -----------------------------------------------------------------------

    fn jiff_to_chrono(ts: k8s_openapi::jiff::Timestamp) -> chrono::DateTime<chrono::Utc> {
        let secs = ts.as_second();
        let nanos = ts.subsec_nanosecond().max(0) as u32;
        chrono::DateTime::from_timestamp(secs, nanos).unwrap_or_else(chrono::Utc::now)
    }

    // -----------------------------------------------------------------------
    // Scale + wait helpers
    // -----------------------------------------------------------------------

    async fn scale(&self, id: &InstanceId, replicas: i32) -> Result<()> {
        let sts_api: Api<StatefulSet> = Api::namespaced(self.client.clone(), &self.namespace);
        sts_api
            .patch(
                &id.0,
                &PatchParams::apply(MANAGER).force(),
                &Patch::Apply(serde_json::json!({
                    "apiVersion": "apps/v1",
                    "kind": "StatefulSet",
                    "metadata": { "name": id.0 },
                    "spec": { "replicas": replicas }
                })),
            )
            .await
            .map_err(|e| Self::classify_err(&id.0, e))?;
        Ok(())
    }

    async fn wait_ready(&self, id: &InstanceId) -> Result<InstanceStatus> {
        let deadline = std::time::Instant::now() + SCALE_TIMEOUT;
        loop {
            let s = self.status(id).await?;
            match s.state {
                InstanceState::Running => return Ok(s),
                InstanceState::Failed => {
                    return Err(ComputeError::Internal(format!(
                        "pod '{}' failed during startup",
                        Self::pod_name(id)
                    )));
                }
                _ => {}
            }
            if std::time::Instant::now() > deadline {
                return Err(ComputeError::Internal(format!(
                    "timeout waiting for '{}' to become ready",
                    id.0
                )));
            }
            tokio::time::sleep(Duration::from_millis(POLL_INTERVAL_MS)).await;
        }
    }

    async fn wait_stopped(&self, id: &InstanceId) -> Result<InstanceStatus> {
        let deadline = std::time::Instant::now() + SCALE_TIMEOUT;
        loop {
            let s = self.status(id).await?;
            if matches!(s.state, InstanceState::Stopped) {
                return Ok(s);
            }
            if std::time::Instant::now() > deadline {
                return Err(ComputeError::Internal(format!(
                    "timeout waiting for '{}' to stop",
                    id.0
                )));
            }
            tokio::time::sleep(Duration::from_millis(POLL_INTERVAL_MS)).await;
        }
    }

    // -----------------------------------------------------------------------
    // Exec helper
    // -----------------------------------------------------------------------

    async fn exec_in_pod(&self, pod_name: &str, command: &[&str]) -> Result<ExecOutput> {
        self.exec_in_pod_container(pod_name, None, command).await
    }

    async fn exec_in_pod_container(
        &self,
        pod_name: &str,
        container: Option<&str>,
        command: &[&str],
    ) -> Result<ExecOutput> {
        let pods: Api<Pod> = Api::namespaced(self.client.clone(), &self.namespace);
        let mut attach = AttachParams::default()
            .stdout(true)
            .stderr(true)
            .stdin(false);
        if let Some(c) = container {
            attach = attach.container(c);
        }
        let mut attached = pods
            .exec(pod_name, command.iter().copied(), &attach)
            .await
            .map_err(|e| ComputeError::Internal(format!("exec failed: {e}")))?;

        let mut stdout = attached.stdout();
        let mut stderr = attached.stderr();
        let status_rx = attached.take_status();

        // Read stdout and stderr concurrently to avoid deadlocks on full buffers.
        let (stdout_bytes, stderr_bytes) = tokio::join!(
            async {
                let mut buf: Vec<u8> = Vec::new();
                if let Some(ref mut r) = stdout {
                    let _ = r.read_to_end(&mut buf).await;
                }
                buf
            },
            async {
                let mut buf: Vec<u8> = Vec::new();
                if let Some(ref mut r) = stderr {
                    let _ = r.read_to_end(&mut buf).await;
                }
                buf
            }
        );

        let exit_code: i32 = if let Some(rx) = status_rx {
            match rx.await {
                Some(status) if status.status.as_deref() == Some("Success") => 0,
                Some(status) => status.code.unwrap_or(1),
                None => 0,
            }
        } else {
            0
        };

        Ok(ExecOutput {
            exit_code,
            stdout: String::from_utf8_lossy(&stdout_bytes).into_owned(),
            stderr: String::from_utf8_lossy(&stderr_bytes).into_owned(),
        })
    }

    // -----------------------------------------------------------------------
    // Port-forward helper
    // -----------------------------------------------------------------------

    /// Bind a free local port and bridge incoming TCP connections to the pod
    /// via kube portforward. The background task lives for the process lifetime.
    async fn setup_port_forward(&self, pod_name: &str, pod_port: u16) -> Result<u16> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .map_err(ComputeError::Io)?;
        let local_port = listener.local_addr().map_err(ComputeError::Io)?.port();

        let client = self.client.clone();
        let namespace = self.namespace.clone();
        let pod_name = pod_name.to_string();

        tokio::spawn(async move {
            loop {
                let Ok((mut tcp_stream, _)) = listener.accept().await else {
                    break;
                };
                let pods: Api<Pod> = Api::namespaced(client.clone(), &namespace);
                let Ok(mut pf) = pods.portforward(&pod_name, &[pod_port]).await else {
                    break;
                };
                let Some(pf_stream) = pf.take_stream(pod_port) else {
                    break;
                };
                tokio::spawn(async move {
                    let (mut pf_read, mut pf_write) = tokio::io::split(pf_stream);
                    let (mut tcp_read, mut tcp_write) = tcp_stream.split();
                    tokio::select! {
                        _ = tokio::io::copy(&mut tcp_read, &mut pf_write) => {}
                        _ = tokio::io::copy(&mut pf_read, &mut tcp_write) => {}
                    }
                    // Keep pf alive until the stream is fully consumed.
                    drop(pf);
                });
            }
        });

        Ok(local_port)
    }

    // -----------------------------------------------------------------------
    // Read env vars from the running StatefulSet (for connection info)
    // -----------------------------------------------------------------------

    async fn get_sts_env(&self, id: &InstanceId) -> Vec<(String, String)> {
        let sts_api: Api<StatefulSet> = Api::namespaced(self.client.clone(), &self.namespace);
        let Ok(sts) = sts_api.get(&id.0).await else {
            return vec![];
        };
        sts.spec
            .as_ref()
            .and_then(|s| s.template.spec.as_ref())
            .and_then(|s| s.containers.first())
            .and_then(|c| c.env.as_ref())
            .map(|env| {
                env.iter()
                    .filter_map(|e| Some((e.name.clone(), e.value.as_ref()?.clone())))
                    .collect()
            })
            .unwrap_or_default()
    }

    // -----------------------------------------------------------------------
    // Tar unpack (blocking — called via spawn_blocking)
    // -----------------------------------------------------------------------

    fn unpack_tar(reader: impl std::io::Read, dest: &Path) -> std::io::Result<usize> {
        use std::io::{ErrorKind};
        #[cfg(unix)]
        use std::os::unix::fs::PermissionsExt;

        fn chmod(path: &Path, mode: u32) {
            #[cfg(unix)]
            let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode));
            #[cfg(not(unix))]
            let _ = (path, mode);
        }

        if dest.exists() {
            let _ = std::fs::remove_dir_all(dest);
        }
        std::fs::create_dir_all(dest)?;
        chmod(dest, 0o755);

        let mut archive = tar::Archive::new(reader);
        archive.set_preserve_permissions(false);
        let mut files = 0usize;

        for entry in archive.entries()? {
            let mut entry = entry?;
            let ty = entry.header().entry_type();

            let raw_path = entry.path()?.into_owned();
            // Skip the first path component (the directory name from `tar -C parent basename`).
            let stripped: PathBuf = raw_path.components().skip(1).collect();
            if stripped.as_os_str().is_empty() {
                continue;
            }
            if !tar_stripped_path_is_safe(&stripped) {
                return Err(std::io::Error::new(
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
                        std::io::Error::new(
                            ErrorKind::InvalidData,
                            format!("symlink '{}' has no target", stripped.display()),
                        )
                    })?
                    .into_owned();

                if !tar_link_target_is_safe(&raw_target) {
                    tracing::warn!(
                        symlink = %stripped.display(),
                        target = %raw_target.display(),
                        "stream_snapshot: symlink outside data dir; capturing dangling link"
                    );
                }
                if let Some(parent) = dest_path.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                #[cfg(unix)]
                {
                    std::os::unix::fs::symlink(&raw_target, &dest_path)?;
                    continue;
                }
                #[cfg(not(unix))]
                return Err(std::io::Error::new(
                    ErrorKind::Unsupported,
                    "symlinks not supported on this platform",
                ));
            }

            if ty.is_hard_link() {
                let raw_link = entry
                    .header()
                    .link_name()?
                    .ok_or_else(|| {
                        std::io::Error::new(
                            ErrorKind::InvalidData,
                            format!("hard link '{}' has no source", stripped.display()),
                        )
                    })?
                    .into_owned();
                let link_stripped: PathBuf = raw_link.components().skip(1).collect();
                if !tar_stripped_path_is_safe(&link_stripped) {
                    return Err(std::io::Error::new(
                        ErrorKind::InvalidData,
                        format!(
                            "hard link '{}' points to unsafe path",
                            stripped.display()
                        ),
                    ));
                }
                let src = dest.join(&link_stripped);
                if let Some(parent) = dest_path.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::hard_link(&src, &dest_path).or_else(|e| {
                    if e.kind() == ErrorKind::PermissionDenied {
                        Err(e)
                    } else {
                        std::fs::copy(&src, &dest_path).map(|_| ())
                    }
                })?;
                files += 1;
                continue;
            }

            if !ty.is_dir() && !ty.is_file() {
                return Err(std::io::Error::new(
                    ErrorKind::InvalidData,
                    format!(
                        "refusing unexpected tar entry type {:?} for '{}'",
                        ty,
                        stripped.display()
                    ),
                ));
            }

            if ty.is_dir() {
                std::fs::create_dir_all(&dest_path)?;
                chmod(&dest_path, 0o755);
            } else {
                if let Some(parent) = dest_path.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                entry.unpack(&dest_path)?;
                files += 1;
            }
        }

        Ok(files)
    }

    fn bare_pod_phase(pod: &Pod) -> Option<&str> {
        pod.status.as_ref()?.phase.as_deref()
    }

    fn init_container_exit_code(pod: &Pod, name: &str) -> Option<i32> {
        let statuses = pod.status.as_ref()?.init_container_statuses.as_ref()?;
        let st = statuses.iter().find(|s| s.name == name)?;
        let term = st.state.as_ref()?.terminated.as_ref()?;
        Some(term.exit_code)
    }

    fn main_container_exit_code(pod: &Pod, name: &str) -> Option<i32> {
        let statuses = pod.status.as_ref()?.container_statuses.as_ref()?;
        let st = statuses.iter().find(|s| s.name == name)?;
        let term = st.state.as_ref()?.terminated.as_ref()?;
        Some(term.exit_code)
    }

    /// Stream `tar -cf - -C parent basename` from a pod (optionally a non-default container) into `dest`.
    async fn stream_tar_from_pod_path(
        &self,
        pod_name: &str,
        exec_container: Option<&str>,
        pack_src: &Path,
        dest: &Path,
    ) -> Result<()> {
        let pods: Api<Pod> = Api::namespaced(self.client.clone(), &self.namespace);
        let parent = pack_src
            .parent()
            .and_then(|p| p.to_str())
            .unwrap_or("/");
        let basename = pack_src
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(".");
        let tar_cmd = format!("tar -cf - -C {parent} {basename}");
        let tar_args = ["sh", "-c", tar_cmd.as_str()];
        let mut attach = AttachParams::default()
            .stdout(true)
            .stderr(false)
            .stdin(false);
        if let Some(c) = exec_container {
            attach = attach.container(c);
        }
        let mut attached = pods
            .exec(pod_name, tar_args.iter().copied(), &attach)
            .await
            .map_err(|e| ComputeError::Internal(format!("exec tar failed: {e}")))?;

        let mut stdout = attached
            .stdout()
            .ok_or_else(|| ComputeError::Internal("no stdout from tar exec".to_string()))?;

        let dest_path = dest.to_path_buf();
        let (pipe_reader, mut pipe_writer) = std::io::pipe().map_err(ComputeError::Io)?;

        let unpack =
            tokio::task::spawn_blocking(move || Self::unpack_tar(pipe_reader, &dest_path));

        let copy_result: Result<()> = async {
            let mut buf = vec![0u8; 65536];
            loop {
                let n = stdout.read(&mut buf).await.map_err(ComputeError::Io)?;
                if n == 0 {
                    break;
                }
                use std::io::Write;
                pipe_writer.write_all(&buf[..n]).map_err(ComputeError::Io)?;
            }
            Ok(())
        }
        .await;

        drop(pipe_writer);
        drop(attached);

        let files = unpack
            .await
            .map_err(|e| ComputeError::Internal(format!("tar unpack task panicked: {e}")))?
            .map_err(ComputeError::Io)?;

        if files == 0 {
            return Err(ComputeError::Internal(format!(
                "tar stream from pod '{pod_name}' produced an empty archive"
            )));
        }

        if let Err(e) = copy_result {
            match &e {
                ComputeError::Io(ioe) if ioe.kind() == std::io::ErrorKind::BrokenPipe => {}
                _ => return Err(e),
            }
        }

        Ok(())
    }

    fn host_data_tree_has_files(dir: &Path) -> std::io::Result<bool> {
        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            let ft = entry.file_type()?;
            if ft.is_file() {
                return Ok(true);
            }
            if ft.is_dir() && Self::host_data_tree_has_files(&entry.path())? {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// `true` when there are no files under `host_data_dir` (directories only or missing → created).
    /// Used to pick the export capture path (init + sidecar + tar out). Any file uses the import path.
    fn host_dir_is_empty_for_export_capture(dir: &Path) -> Result<bool> {
        if !dir.exists() {
            std::fs::create_dir_all(dir).map_err(ComputeError::Io)?;
            return Ok(true);
        }
        if !dir.is_dir() {
            return Err(ComputeError::Internal(format!(
                "host_data_dir {} is not a directory",
                dir.display()
            )));
        }
        let has_files = Self::host_data_tree_has_files(dir).map_err(ComputeError::Io)?;
        Ok(!has_files)
    }

    /// Stream a tar archive of `host_dir` from the gfs host into the pod via exec stdin (`tar xf`).
    async fn stream_host_dir_tar_into_pod(
        &self,
        pod_name: &str,
        container: &str,
        host_dir: &Path,
        dest_in_container: &Path,
    ) -> Result<()> {
        use std::process::Stdio;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::process::Command;

        let dest = dest_in_container.to_string_lossy();
        let extract_cmd = format!("mkdir -p {dest} && tar xf - -C {dest}");

        let pods: Api<Pod> = Api::namespaced(self.client.clone(), &self.namespace);
        let attach = AttachParams::default()
            .stdin(true)
            .stdout(false)
            .stderr(false)
            .container(container);

        let mut attached = pods
            .exec(
                pod_name,
                ["sh", "-c", extract_cmd.as_str()].iter().copied(),
                &attach,
            )
            .await
            .map_err(|e| ComputeError::Internal(format!("exec tar-in failed: {e}")))?;

        let mut stdin = attached
            .stdin()
            .ok_or_else(|| ComputeError::Internal("tar-in: no stdin on attach".to_string()))?;
        let status_rx = attached.take_status();

        let host = host_dir
            .to_str()
            .ok_or_else(|| ComputeError::Internal("host_data_dir is not valid UTF-8".into()))?
            .to_string();

        let mut child = Command::new("tar")
            .args(["cf", "-", "-C", host.as_str(), "."])
            .stdout(Stdio::piped())
            .spawn()
            .map_err(|e| ComputeError::Internal(format!("host tar spawn failed: {e}")))?;

        let mut tar_out = child
            .stdout
            .take()
            .ok_or_else(|| ComputeError::Internal("host tar: no stdout".to_string()))?;

        let copy_res: std::result::Result<(), ComputeError> = async {
            let mut buf = vec![0u8; 65536];
            loop {
                let n = tar_out.read(&mut buf).await.map_err(ComputeError::Io)?;
                if n == 0 {
                    break;
                }
                stdin.write_all(&buf[..n]).await.map_err(ComputeError::Io)?;
            }
            stdin.shutdown().await.map_err(ComputeError::Io)?;
            Ok(())
        }
        .await;

        drop(stdin);
        drop(tar_out);

        if let Err(e) = copy_res {
            drop(attached);
            return Err(e);
        }

        let exit_code: i32 = if let Some(rx) = status_rx {
            match rx.await {
                Some(status) if status.status.as_deref() == Some("Success") => 0,
                Some(status) => status.code.unwrap_or(1),
                None => 0,
            }
        } else {
            0
        };

        let host_tar_status = child.wait().await.map_err(ComputeError::Io)?;
        if !host_tar_status.success() {
            drop(attached);
            return Err(ComputeError::Internal(format!(
                "host tar cf failed (status {host_tar_status})"
            )));
        }

        drop(attached);

        if exit_code != 0 {
            return Err(ComputeError::Internal(format!(
                "pod tar extract failed with exit {exit_code}"
            )));
        }

        Ok(())
    }

    async fn task_pod_logs(pods: &Api<Pod>, name: &str, container: &str) -> String {
        let lp = LogParams {
            container: Some(container.to_string()),
            ..Default::default()
        };
        pods.logs(name, &lp).await.unwrap_or_default()
    }
}

// ---------------------------------------------------------------------------
// Compute implementation
// ---------------------------------------------------------------------------

#[async_trait]
impl Compute for K8sCompute {
    // -----------------------------------------------------------------------
    // provision
    // -----------------------------------------------------------------------

    #[instrument(skip(self, definition))]
    async fn provision(&self, definition: &ComputeDefinition) -> Result<InstanceId> {
        let id_str = Self::generate_name(&definition.image);
        let id = InstanceId(id_str.clone());
        let labels = self.app_labels(&id);
        let pvc_name = Self::pvc_name(&id);

        let meta = |name: &str| ObjectMeta {
            name: Some(name.to_string()),
            namespace: Some(self.namespace.clone()),
            labels: Some(labels.clone()),
            ..Default::default()
        };

        // 1. PVC
        let pvc = PersistentVolumeClaim {
            metadata: meta(&pvc_name),
            spec: Some(PersistentVolumeClaimSpec {
                access_modes: Some(vec!["ReadWriteOnce".to_string()]),
                storage_class_name: self.storage_class.clone(),
                resources: Some(VolumeResourceRequirements {
                    requests: Some(
                        [("storage".to_string(), Quantity(DEFAULT_PVC_SIZE.to_string()))].into(),
                    ),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        let pvcs: Api<PersistentVolumeClaim> =
            Api::namespaced(self.client.clone(), &self.namespace);
        pvcs.create(&PostParams::default(), &pvc)
            .await
            .map_err(|e| ComputeError::Internal(format!("failed to create PVC: {e}")))?;

        // 2. Service
        let svc_ports: Vec<ServicePort> = definition
            .ports
            .iter()
            .enumerate()
            .map(|(i, p)| ServicePort {
                name: Some(format!("port-{}", i)),
                port: p.compute_port as i32,
                target_port: Some(IntOrString::Int(p.compute_port as i32)),
                protocol: Some("TCP".to_string()),
                ..Default::default()
            })
            .collect();

        let svc = Service {
            metadata: meta(&id_str),
            spec: Some(ServiceSpec {
                selector: Some(labels.clone()),
                ports: Some(svc_ports),
                type_: Some("ClusterIP".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let svcs: Api<Service> = Api::namespaced(self.client.clone(), &self.namespace);
        svcs.create(&PostParams::default(), &svc)
            .await
            .map_err(|e| ComputeError::Internal(format!("failed to create Service: {e}")))?;

        // 3. StatefulSet
        let k8s_env: Vec<K8sEnvVar> = definition
            .env
            .iter()
            .map(|e| K8sEnvVar {
                name: e.name.clone(),
                value: e.default.clone(),
                ..Default::default()
            })
            .collect();

        let data_dir = definition.data_dir.to_string_lossy().into_owned();

        let container = Container {
            name: "db".to_string(),
            image: Some(definition.image.clone()),
            env: Some(k8s_env),
            ports: Some(
                definition
                    .ports
                    .iter()
                    .enumerate()
                    .map(|(i, p)| k8s_openapi::api::core::v1::ContainerPort {
                        name: Some(format!("port-{}", i)),
                        container_port: p.compute_port as i32,
                        protocol: Some("TCP".to_string()),
                        ..Default::default()
                    })
                    .collect(),
            ),
            volume_mounts: Some(vec![VolumeMount {
                name: "data".to_string(),
                mount_path: data_dir,
                ..Default::default()
            }]),
            args: if definition.args.is_empty() {
                None
            } else {
                Some(definition.args.clone())
            },
            ..Default::default()
        };

        let sts = StatefulSet {
            metadata: meta(&id_str),
            spec: Some(StatefulSetSpec {
                replicas: Some(1),
                service_name: Some(id_str.clone()),
                selector: LabelSelector {
                    match_labels: Some(labels.clone()),
                    ..Default::default()
                },
                template: PodTemplateSpec {
                    metadata: Some(ObjectMeta {
                        labels: Some(labels.clone()),
                        ..Default::default()
                    }),
                    spec: Some(PodSpec {
                        containers: vec![container],
                        volumes: Some(vec![Volume {
                            name: "data".to_string(),
                            persistent_volume_claim: Some(PersistentVolumeClaimVolumeSource {
                                claim_name: pvc_name,
                                read_only: Some(false),
                            }),
                            ..Default::default()
                        }]),
                        ..Default::default()
                    }),
                },
                ..Default::default()
            }),
            ..Default::default()
        };

        let sts_api: Api<StatefulSet> = Api::namespaced(self.client.clone(), &self.namespace);
        sts_api
            .create(&PostParams::default(), &sts)
            .await
            .map_err(|e| ComputeError::Internal(format!("failed to create StatefulSet: {e}")))?;

        tracing::info!(id = id_str, "provisioned k8s StatefulSet+Service+PVC");
        Ok(id)
    }

    // -----------------------------------------------------------------------
    // start
    // -----------------------------------------------------------------------

    #[instrument(skip(self, options))]
    async fn start(&self, id: &InstanceId, options: StartOptions) -> Result<InstanceStatus> {
        self.scale(id, 1).await?;
        if options.wait {
            self.wait_ready(id).await
        } else {
            self.status(id).await
        }
    }

    // -----------------------------------------------------------------------
    // stop
    // -----------------------------------------------------------------------

    #[instrument(skip(self))]
    async fn stop(&self, id: &InstanceId) -> Result<InstanceStatus> {
        self.scale(id, 0).await?;
        self.wait_stopped(id).await
    }

    // -----------------------------------------------------------------------
    // restart
    // -----------------------------------------------------------------------

    #[instrument(skip(self))]
    async fn restart(&self, id: &InstanceId) -> Result<InstanceStatus> {
        self.scale(id, 0).await?;
        self.wait_stopped(id).await?;
        self.scale(id, 1).await?;
        self.wait_ready(id).await
    }

    // -----------------------------------------------------------------------
    // status
    // -----------------------------------------------------------------------

    #[instrument(skip(self))]
    async fn status(&self, id: &InstanceId) -> Result<InstanceStatus> {
        let sts_api: Api<StatefulSet> = Api::namespaced(self.client.clone(), &self.namespace);
        let sts = sts_api
            .get(&id.0)
            .await
            .map_err(|e| Self::classify_err(&id.0, e))?;

        let desired = sts.spec.as_ref().and_then(|s| s.replicas).unwrap_or(0);
        let ready = sts
            .status
            .as_ref()
            .and_then(|s| s.ready_replicas)
            .unwrap_or(0);
        let current = sts
            .status
            .as_ref()
            .and_then(|s| s.current_replicas)
            .unwrap_or(0);

        let sts_state = match (desired, current, ready) {
            (0, 0, _) => InstanceState::Stopped,
            (0, _, _) => InstanceState::Stopping,
            (d, _, r) if d > 0 && r >= d => InstanceState::Running,
            (d, c, _) if d > 0 && c > 0 => InstanceState::Starting,
            _ => InstanceState::Starting,
        };

        // Refine state using the actual pod phase.
        let pod_name = Self::pod_name(id);
        let pods: Api<Pod> = Api::namespaced(self.client.clone(), &self.namespace);
        let (started_at, state) = match pods.get_opt(&pod_name).await {
            Ok(Some(pod)) => {
                let started = pod
                    .status
                    .as_ref()
                    .and_then(|s| s.start_time.as_ref())
                    .map(|t| Self::jiff_to_chrono(t.0));

                let phase = pod
                    .status
                    .as_ref()
                    .and_then(|s| s.phase.as_deref())
                    .unwrap_or("Unknown");

                let pod_state = match phase {
                    "Running" => {
                        let container_ready = pod
                            .status
                            .as_ref()
                            .and_then(|s| s.container_statuses.as_ref())
                            .and_then(|cs| cs.first())
                            .map(|cs| cs.ready)
                            .unwrap_or(false);
                        if container_ready {
                            InstanceState::Running
                        } else {
                            InstanceState::Starting
                        }
                    }
                    "Pending" => InstanceState::Starting,
                    "Succeeded" | "Completed" => InstanceState::Stopped,
                    "Failed" => InstanceState::Failed,
                    _ => sts_state,
                };

                (started, pod_state)
            }
            _ => (None, sts_state),
        };

        Ok(InstanceStatus {
            id: InstanceId(pod_name),
            state,
            pid: None,
            started_at,
            exit_code: None,
        })
    }

    // -----------------------------------------------------------------------
    // get_connection_info  (port-forward to localhost)
    // -----------------------------------------------------------------------

    #[instrument(skip(self))]
    async fn get_connection_info(
        &self,
        id: &InstanceId,
        compute_port: u16,
    ) -> Result<InstanceConnectionInfo> {
        let pod_name = Self::pod_name(id);
        let local_port = self.setup_port_forward(&pod_name, compute_port).await?;
        let env = self.get_sts_env(id).await;
        Ok(InstanceConnectionInfo {
            host: "127.0.0.1".to_string(),
            port: local_port,
            env,
        })
    }

    // -----------------------------------------------------------------------
    // get_task_connection_info  (in-cluster Service DNS)
    // -----------------------------------------------------------------------

    async fn get_task_connection_info(
        &self,
        id: &InstanceId,
        compute_port: u16,
    ) -> Result<InstanceConnectionInfo> {
        let env = self.get_sts_env(id).await;
        Ok(InstanceConnectionInfo {
            host: format!("{}.{}.svc.cluster.local", id.0, self.namespace),
            port: compute_port,
            env,
        })
    }

    // -----------------------------------------------------------------------
    // prepare_for_snapshot
    // -----------------------------------------------------------------------

    #[instrument(skip(self, commands))]
    async fn prepare_for_snapshot(&self, id: &InstanceId, commands: &[String]) -> Result<()> {
        let pod_name = Self::pod_name(id);
        for cmd in commands {
            if cmd.trim().is_empty() {
                continue;
            }
            let out = self.exec_in_pod(&pod_name, &["sh", "-c", cmd]).await?;
            if out.exit_code != 0 {
                return Err(ComputeError::Internal(format!(
                    "prepare_for_snapshot command failed (exit {}): {}\nstderr: {}",
                    out.exit_code,
                    cmd,
                    out.stderr.trim()
                )));
            }
        }
        Ok(())
    }

    // -----------------------------------------------------------------------
    // capabilities
    // -----------------------------------------------------------------------

    async fn capabilities(&self) -> Result<ComputeCapabilities> {
        Ok(ComputeCapabilities {
            supports_stream_snapshot: true,
            supports_exec_as_root: false,
        })
    }

    // -----------------------------------------------------------------------
    // exec
    // -----------------------------------------------------------------------

    async fn exec(
        &self,
        id: &InstanceId,
        command: &str,
        user: Option<&str>,
    ) -> Result<ExecOutput> {
        if user.is_some() {
            return Err(ComputeError::Internal(
                "exec with user override is not supported by the k8s compute runtime".to_string(),
            ));
        }
        let pod_name = Self::pod_name(id);
        self.exec_in_pod(&pod_name, &["sh", "-c", command]).await
    }

    // -----------------------------------------------------------------------
    // describe_runtime
    // -----------------------------------------------------------------------

    async fn describe_runtime(&self) -> Result<RuntimeDescriptor> {
        let version = self
            .client
            .apiserver_version()
            .await
            .map(|v| v.git_version)
            .unwrap_or_else(|_| "unknown".to_string());
        Ok(RuntimeDescriptor {
            provider: "k8s".to_string(),
            version,
        })
    }

    // -----------------------------------------------------------------------
    // logs
    // -----------------------------------------------------------------------

    #[instrument(skip(self))]
    async fn logs(&self, id: &InstanceId, options: LogsOptions) -> Result<Vec<LogEntry>> {
        let pod_name = Self::pod_name(id);
        let pods: Api<Pod> = Api::namespaced(self.client.clone(), &self.namespace);

        let since_seconds = options.since.map(|dt| {
            chrono::Utc::now()
                .signed_duration_since(dt)
                .num_seconds()
                .max(0)
        });

        let log_params = LogParams {
            tail_lines: options.tail.map(|n| n as i64),
            since_seconds,
            timestamps: true,
            follow: false,
            ..Default::default()
        };

        let raw = match pods.logs(&pod_name, &log_params).await {
            Ok(r) => r,
            // Pod not yet running (ContainerCreating, Pending, etc.) — return empty.
            Err(kube::Error::Api(ref resp)) if resp.code == 400 => return Ok(vec![]),
            Err(e) => return Err(Self::classify_err(&pod_name, e)),
        };

        let mut entries = Vec::new();
        for line in raw.lines() {
            // k8s log lines with timestamps: "2024-01-01T00:00:00.000000000Z <message>"
            let parts: Vec<&str> = line.splitn(2, ' ').collect();
            let (timestamp, message) = if parts.len() == 2 {
                let ts = chrono::DateTime::parse_from_rfc3339(parts[0])
                    .map(|dt| dt.with_timezone(&chrono::Utc))
                    .unwrap_or_else(|_| chrono::Utc::now());
                (ts, parts[1].to_owned())
            } else {
                (chrono::Utc::now(), line.to_owned())
            };

            let stream = if options.stderr {
                LogStream::Stderr
            } else {
                LogStream::Stdout
            };
            entries.push(LogEntry {
                timestamp,
                stream,
                message,
            });
        }

        Ok(entries)
    }

    // -----------------------------------------------------------------------
    // pause / unpause  (k8s has no equivalent to cgroup freeze)
    // -----------------------------------------------------------------------

    async fn pause(&self, _id: &InstanceId) -> Result<InstanceStatus> {
        Err(ComputeError::PauseUnsupported(
            "Kubernetes does not support cgroup-level pause; use 'gfs compute stop' instead"
                .to_string(),
        ))
    }

    async fn unpause(&self, _id: &InstanceId) -> Result<InstanceStatus> {
        Err(ComputeError::PauseUnsupported(
            "Kubernetes does not support cgroup-level pause; use 'gfs compute start' instead"
                .to_string(),
        ))
    }

    // -----------------------------------------------------------------------
    // get_instance_data_mount_host_path  (data is in-cluster PVC, no host path)
    // -----------------------------------------------------------------------

    async fn get_instance_data_mount_host_path(
        &self,
        _id: &InstanceId,
        _compute_data_path: &str,
    ) -> Result<Option<PathBuf>> {
        Ok(None)
    }

    // -----------------------------------------------------------------------
    // remove_instance  (StatefulSet + Service deleted; PVC retained for safety)
    // -----------------------------------------------------------------------

    #[instrument(skip(self))]
    async fn remove_instance(&self, id: &InstanceId) -> Result<()> {
        let dp = DeleteParams::default();
        let sts_api: Api<StatefulSet> = Api::namespaced(self.client.clone(), &self.namespace);
        let _ = sts_api.delete(&id.0, &dp).await;
        let svc_api: Api<Service> = Api::namespaced(self.client.clone(), &self.namespace);
        let _ = svc_api.delete(&id.0, &dp).await;
        tracing::info!(id = id.0, "removed k8s StatefulSet and Service (PVC retained)");
        Ok(())
    }

    // -----------------------------------------------------------------------
    // stream_snapshot  (exec tar inside pod → pipe → unpack on host)
    // -----------------------------------------------------------------------

    #[instrument(skip(self))]
    async fn stream_snapshot(
        &self,
        id: &InstanceId,
        container_path: &str,
        dest: &Path,
    ) -> Result<()> {
        let pod_name = Self::pod_name(id);
        self.stream_tar_from_pod_path(&pod_name, None, Path::new(container_path), dest)
            .await?;
        tracing::info!(pod = pod_name, container_path, "stream_snapshot complete");
        Ok(())
    }

    // -----------------------------------------------------------------------
    // run_task  (ephemeral pod for import/export)
    // -----------------------------------------------------------------------

    #[instrument(skip(self, definition, command))]
    async fn run_task(
        &self,
        definition: &ComputeDefinition,
        command: &str,
        _linked_to: Option<&InstanceId>,
    ) -> Result<ExecOutput> {
        let task_name = format!(
            "gfs-task-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis()
        );

        let pods: Api<Pod> = Api::namespaced(self.client.clone(), &self.namespace);

        let mut meta_labels = BTreeMap::from([
            ("managed-by".to_string(), MANAGER.to_string()),
            ("app".to_string(), task_name.clone()),
        ]);
        meta_labels.extend(self.extra_labels.clone());

        let k8s_env: Vec<K8sEnvVar> = definition
            .env
            .iter()
            .map(|e| K8sEnvVar {
                name: e.name.clone(),
                value: e.default.clone(),
                ..Default::default()
            })
            .collect();

        let data_dir = definition.data_dir.to_string_lossy().into_owned();
        let data_mount = VolumeMount {
            name: "data".to_string(),
            mount_path: data_dir.clone(),
            ..Default::default()
        };

        let empty_vol = Volume {
            name: "data".to_string(),
            empty_dir: Some(k8s_openapi::api::core::v1::EmptyDirVolumeSource {
                ..Default::default()
            }),
            ..Default::default()
        };

        let host_capture_export = match definition.host_data_dir.as_ref() {
            None => false,
            Some(dir) => Self::host_dir_is_empty_for_export_capture(dir)?,
        };

        if host_capture_export {
            // Export-style: init runs the tool; busybox sidecar keeps the pod alive so we can
            // `tar` the shared EmptyDir out to the host (bind mounts are not available on k8s).
            let init_task = Container {
                name: TASK_CONTAINER_NAME.to_string(),
                image: Some(definition.image.clone()),
                env: Some(k8s_env.clone()),
                command: Some(vec!["sh".to_string(), "-c".to_string()]),
                args: Some(vec![command.to_string()]),
                volume_mounts: Some(vec![data_mount.clone()]),
                ..Default::default()
            };

            let sidecar = Container {
                name: SIDECAR_CONTAINER_NAME.to_string(),
                image: Some(BUSYBOX_SIDECAR_IMAGE.to_string()),
                command: Some(vec!["sleep".to_string(), "infinity".to_string()]),
                volume_mounts: Some(vec![data_mount]),
                ..Default::default()
            };

            let task_pod = Pod {
                metadata: ObjectMeta {
                    name: Some(task_name.clone()),
                    namespace: Some(self.namespace.clone()),
                    labels: Some(meta_labels),
                    ..Default::default()
                },
                spec: Some(PodSpec {
                    restart_policy: Some("Never".to_string()),
                    init_containers: Some(vec![init_task]),
                    containers: vec![sidecar],
                    volumes: Some(vec![empty_vol]),
                    ..Default::default()
                }),
                ..Default::default()
            };

            pods.create(&PostParams::default(), &task_pod)
                .await
                .map_err(|e| ComputeError::Internal(format!("failed to create task pod: {e}")))?;

            let deadline = std::time::Instant::now() + Duration::from_secs(600);
            loop {
                let pod = pods
                    .get(&task_name)
                    .await
                    .map_err(|e| ComputeError::Internal(format!("task pod get failed: {e}")))?;

                if let Some(code) = Self::init_container_exit_code(&pod, TASK_CONTAINER_NAME) {
                    let raw =
                        Self::task_pod_logs(&pods, &task_name, TASK_CONTAINER_NAME).await;
                    if code != 0 {
                        let _ = pods.delete(&task_name, &DeleteParams::default()).await;
                        return Ok(ExecOutput {
                            exit_code: code,
                            stdout: raw,
                            stderr: String::new(),
                        });
                    }
                    break;
                }

                if Self::bare_pod_phase(&pod) == Some("Failed") {
                    let raw =
                        Self::task_pod_logs(&pods, &task_name, TASK_CONTAINER_NAME).await;
                    let code = Self::init_container_exit_code(&pod, TASK_CONTAINER_NAME).unwrap_or(1);
                    let _ = pods.delete(&task_name, &DeleteParams::default()).await;
                    return Ok(ExecOutput {
                        exit_code: code,
                        stdout: raw,
                        stderr: String::new(),
                    });
                }

                if std::time::Instant::now() > deadline {
                    let _ = pods.delete(&task_name, &DeleteParams::default()).await;
                    return Err(ComputeError::Internal(format!(
                        "task pod '{task_name}' timed out waiting for init container"
                    )));
                }
                tokio::time::sleep(Duration::from_millis(POLL_INTERVAL_MS)).await;
            }

            // Sidecar must be exec-ready before we stream from it.
            let sidecar_deadline = std::time::Instant::now() + Duration::from_secs(120);
            loop {
                if self
                    .exec_in_pod_container(
                        &task_name,
                        Some(SIDECAR_CONTAINER_NAME),
                        &["true"],
                    )
                    .await
                    .is_ok()
                {
                    break;
                }
                if std::time::Instant::now() > sidecar_deadline {
                    let _ = pods.delete(&task_name, &DeleteParams::default()).await;
                    return Err(ComputeError::Internal(
                        "task pod sidecar did not become executable".to_string(),
                    ));
                }
                tokio::time::sleep(Duration::from_millis(POLL_INTERVAL_MS)).await;
            }

            let host_dir = definition.host_data_dir.as_ref().unwrap();
            if let Err(e) = self
                .stream_tar_from_pod_path(
                    &task_name,
                    Some(SIDECAR_CONTAINER_NAME),
                    &definition.data_dir,
                    host_dir,
                )
                .await
            {
                let _ = pods.delete(&task_name, &DeleteParams::default()).await;
                return Err(e);
            }

            let raw = Self::task_pod_logs(&pods, &task_name, TASK_CONTAINER_NAME).await;
            let _ = pods.delete(&task_name, &DeleteParams::default()).await;
            return Ok(ExecOutput {
                exit_code: 0,
                stdout: raw,
                stderr: String::new(),
            });
        }

        if let Some(host_dir) = definition.host_data_dir.as_ref() {
            // Import-style: staged files on the host must be copied into the EmptyDir before the
            // tool runs. We use a long-running main container, inject a tar stream, then exec.
            let main = Container {
                name: TASK_CONTAINER_NAME.to_string(),
                image: Some(definition.image.clone()),
                env: Some(k8s_env),
                command: Some(vec!["sleep".to_string(), "infinity".to_string()]),
                volume_mounts: Some(vec![data_mount]),
                ..Default::default()
            };

            let task_pod = Pod {
                metadata: ObjectMeta {
                    name: Some(task_name.clone()),
                    namespace: Some(self.namespace.clone()),
                    labels: Some(meta_labels),
                    ..Default::default()
                },
                spec: Some(PodSpec {
                    restart_policy: Some("Never".to_string()),
                    containers: vec![main],
                    volumes: Some(vec![empty_vol]),
                    ..Default::default()
                }),
                ..Default::default()
            };

            pods.create(&PostParams::default(), &task_pod)
                .await
                .map_err(|e| ComputeError::Internal(format!("failed to create task pod: {e}")))?;

            let deadline = std::time::Instant::now() + Duration::from_secs(600);
            loop {
                let pod = pods
                    .get(&task_name)
                    .await
                    .map_err(|e| ComputeError::Internal(format!("task pod get failed: {e}")))?;
                if Self::bare_pod_phase(&pod) == Some("Running") {
                    let ready = pod
                        .status
                        .as_ref()
                        .and_then(|s| s.container_statuses.as_ref())
                        .and_then(|cs| cs.iter().find(|c| c.name == TASK_CONTAINER_NAME))
                        .map(|c| c.ready)
                        .unwrap_or(false);
                    if ready {
                        break;
                    }
                }
                if Self::bare_pod_phase(&pod) == Some("Failed") {
                    let raw =
                        Self::task_pod_logs(&pods, &task_name, TASK_CONTAINER_NAME).await;
                    let _ = pods.delete(&task_name, &DeleteParams::default()).await;
                    return Ok(ExecOutput {
                        exit_code: 1,
                        stdout: raw,
                        stderr: String::new(),
                    });
                }
                if std::time::Instant::now() > deadline {
                    let _ = pods.delete(&task_name, &DeleteParams::default()).await;
                    return Err(ComputeError::Internal(format!(
                        "task pod '{task_name}' timed out waiting for main container"
                    )));
                }
                tokio::time::sleep(Duration::from_millis(POLL_INTERVAL_MS)).await;
            }

            self.stream_host_dir_tar_into_pod(&task_name, TASK_CONTAINER_NAME, host_dir, &definition.data_dir)
                .await?;

            let out = self
                .exec_in_pod_container(
                    &task_name,
                    Some(TASK_CONTAINER_NAME),
                    &["sh", "-c", command],
                )
                .await?;

            let _ = pods.delete(&task_name, &DeleteParams::default()).await;
            return Ok(out);
        }

        // No host_data_dir: single short-lived container.
        let container = Container {
            name: TASK_CONTAINER_NAME.to_string(),
            image: Some(definition.image.clone()),
            env: Some(k8s_env),
            command: Some(vec!["sh".to_string(), "-c".to_string()]),
            args: Some(vec![command.to_string()]),
            volume_mounts: Some(vec![data_mount]),
            ..Default::default()
        };

        let task_pod = Pod {
            metadata: ObjectMeta {
                name: Some(task_name.clone()),
                namespace: Some(self.namespace.clone()),
                labels: Some(meta_labels),
                ..Default::default()
            },
            spec: Some(PodSpec {
                restart_policy: Some("Never".to_string()),
                containers: vec![container],
                volumes: Some(vec![empty_vol]),
                ..Default::default()
            }),
            ..Default::default()
        };

        pods.create(&PostParams::default(), &task_pod)
            .await
            .map_err(|e| ComputeError::Internal(format!("failed to create task pod: {e}")))?;

        let deadline = std::time::Instant::now() + Duration::from_secs(600);
        loop {
            let pod = pods
                .get(&task_name)
                .await
                .map_err(|e| ComputeError::Internal(format!("task pod get failed: {e}")))?;

            match Self::bare_pod_phase(&pod) {
                Some("Succeeded") => {
                    let code = Self::main_container_exit_code(&pod, TASK_CONTAINER_NAME).unwrap_or(0);
                    let raw =
                        Self::task_pod_logs(&pods, &task_name, TASK_CONTAINER_NAME).await;
                    let _ = pods.delete(&task_name, &DeleteParams::default()).await;
                    return Ok(ExecOutput {
                        exit_code: code,
                        stdout: raw,
                        stderr: String::new(),
                    });
                }
                Some("Failed") => {
                    let code = Self::main_container_exit_code(&pod, TASK_CONTAINER_NAME).unwrap_or(1);
                    let raw =
                        Self::task_pod_logs(&pods, &task_name, TASK_CONTAINER_NAME).await;
                    let _ = pods.delete(&task_name, &DeleteParams::default()).await;
                    return Ok(ExecOutput {
                        exit_code: code,
                        stdout: raw,
                        stderr: String::new(),
                    });
                }
                _ => {}
            }

            if std::time::Instant::now() > deadline {
                let _ = pods.delete(&task_name, &DeleteParams::default()).await;
                return Err(ComputeError::Internal(format!(
                    "task pod '{task_name}' timed out"
                )));
            }
            tokio::time::sleep(Duration::from_millis(POLL_INTERVAL_MS)).await;
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pod_name_appends_zero() {
        let id = InstanceId("gfs-postgres-123".to_string());
        assert_eq!(K8sCompute::pod_name(&id), "gfs-postgres-123-0");
    }

    #[test]
    fn pvc_name_appends_data() {
        let id = InstanceId("gfs-postgres-123".to_string());
        assert_eq!(K8sCompute::pvc_name(&id), "gfs-postgres-123-data");
    }

    #[test]
    fn generate_name_uses_correct_prefix() {
        assert!(K8sCompute::generate_name("postgres:17").starts_with("gfs-postgres-"));
        assert!(K8sCompute::generate_name("mysql:8").starts_with("gfs-mysql-"));
        assert!(K8sCompute::generate_name("clickhouse:latest").starts_with("gfs-clickhouse-"));
    }

    #[test]
    fn tar_stripped_path_safety() {
        assert!(tar_stripped_path_is_safe(Path::new("base")));
        assert!(tar_stripped_path_is_safe(Path::new("global/pg_control")));
        assert!(!tar_stripped_path_is_safe(Path::new("")));
        assert!(!tar_stripped_path_is_safe(Path::new("../escape")));
        assert!(!tar_stripped_path_is_safe(Path::new("/absolute")));
    }

    #[test]
    fn tar_link_target_safety() {
        assert!(tar_link_target_is_safe(Path::new("pg_wal")));
        assert!(!tar_link_target_is_safe(Path::new("")));
        assert!(!tar_link_target_is_safe(Path::new("/absolute")));
        assert!(!tar_link_target_is_safe(Path::new("../escape")));
    }
}
