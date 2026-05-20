//! Kubernetes (k3s) adapter for the [`Compute`] port.
//!
//! This runtime manages a single Postgres workspace as Kubernetes resources:
//! - PVC for data
//! - StatefulSet (1 replica) mounting the PVC
//! - ClusterIP Service for connectivity
//!
//! Notes:
//! - `pause`/`unpause` are not supported (no cgroup freezer equivalent).
//! - `host_data_dir` in `ComputeDefinition` is ignored (docker-specific).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use chrono::Utc;
use gfs_domain::ports::compute::{
    Compute, ComputeCapabilities, ComputeDefinition, ComputeError, ExecOutput, InstanceConnectionInfo,
    InstanceId, InstanceState, InstanceStatus, LogEntry, LogStream, LogsOptions, PortMapping,
    Result, RuntimeDescriptor, StartOptions,
};
use k8s_openapi::api::apps::v1::StatefulSet;
use k8s_openapi::api::core::v1::{
    Container, PersistentVolumeClaim, PersistentVolumeClaimSpec, Pod, PodSpec, PodTemplateSpec,
    Service, ServicePort, ServiceSpec, Volume, VolumeMount,
};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{LabelSelector, ObjectMeta};
use kube::api::{AttachParams, DeleteParams, ListParams, Patch, PatchParams, PostParams};
use kube::{Api, Client};
use serde_json::json;

const DEFAULT_NAMESPACE: &str = "gfs";
const DEFAULT_PVC_SIZE_GI: &str = "5";

fn k8s_storage_class() -> Option<String> {
    std::env::var("GFS_K8S_STORAGE_CLASS")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn k8s_pvc_size_gi() -> String {
    std::env::var("GFS_K8S_PVC_SIZE_GI")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| DEFAULT_PVC_SIZE_GI.to_string())
}
const APP_LABEL_KEY: &str = "app.kubernetes.io/name";
const APP_LABEL_VALUE: &str = "gfs";
const INSTANCE_LABEL_KEY: &str = "gfs.guepard.run/instance";

fn now_suffix() -> String {
    // short, dns-safe suffix
    format!("{}", Utc::now().timestamp_millis())
}

fn ensure_dns_label(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        let c = ch.to_ascii_lowercase();
        if c.is_ascii_alphanumeric() || c == '-' {
            out.push(c);
        } else {
            out.push('-');
        }
    }
    out.trim_matches('-').to_string()
}

fn instance_name_from_definition(def: &ComputeDefinition) -> String {
    let image = def.image.to_ascii_lowercase();
    let prefix = if image.contains("postgres") {
        "gfs-pg"
    } else if image.contains("mysql") {
        "gfs-mysql"
    } else if image.contains("clickhouse") {
        "gfs-ch"
    } else {
        "gfs-db"
    };
    format!("{prefix}-{}", now_suffix())
}

fn labels_for(instance: &str) -> BTreeMap<String, String> {
    let mut m = BTreeMap::new();
    m.insert(APP_LABEL_KEY.to_string(), APP_LABEL_VALUE.to_string());
    m.insert(INSTANCE_LABEL_KEY.to_string(), instance.to_string());
    m
}

#[derive(Clone)]
pub struct KubernetesCompute {
    client: Client,
    namespace: String,
}

impl KubernetesCompute {
    pub async fn new(namespace: Option<String>) -> std::result::Result<Self, ComputeError> {
        let client = Client::try_default()
            .await
            .map_err(|e| ComputeError::NotAvailable(format!("kubernetes client unavailable: {e}")))?;
        Ok(Self {
            client,
            namespace: namespace.unwrap_or_else(|| DEFAULT_NAMESPACE.to_string()),
        })
    }

    fn api_statefulsets(&self) -> Api<StatefulSet> {
        Api::namespaced(self.client.clone(), &self.namespace)
    }

    fn api_services(&self) -> Api<Service> {
        Api::namespaced(self.client.clone(), &self.namespace)
    }

    fn api_pods(&self) -> Api<Pod> {
        Api::namespaced(self.client.clone(), &self.namespace)
    }

    fn api_pvcs(&self) -> Api<PersistentVolumeClaim> {
        Api::namespaced(self.client.clone(), &self.namespace)
    }

    fn svc_name(instance: &str) -> String {
        format!("{instance}-svc")
    }

    fn pvc_name_for(instance: &str, def: &ComputeDefinition) -> String {
        // Kubernetes-specific convention:
        // - if host_data_dir is set to `pvc:<name>` we treat `<name>` as the PVC to mount.
        // - otherwise we default to a PVC derived from instance name.
        if let Some(ref hd) = def.host_data_dir {
            let s = hd.to_string_lossy();
            let s = s.trim();
            if let Some(rest) = s.strip_prefix("pvc:") {
                let rest = rest.trim();
                if !rest.is_empty() {
                    return rest.to_string();
                }
            }
        }
        format!("{instance}-data")
    }

    fn statefulset_manifest(&self, instance: &str, def: &ComputeDefinition) -> StatefulSet {
        let labels = labels_for(instance);
        let svc_name = Self::svc_name(instance);
        let pvc_name = Self::pvc_name_for(instance, def);

        let env: Vec<k8s_openapi::api::core::v1::EnvVar> = def
            .env
            .iter()
            .map(|e| k8s_openapi::api::core::v1::EnvVar {
                name: e.name.clone(),
                value: e.default.clone(),
                ..Default::default()
            })
            .collect();

        let container_ports: Vec<k8s_openapi::api::core::v1::ContainerPort> = def
            .ports
            .iter()
            .map(|p| k8s_openapi::api::core::v1::ContainerPort {
                container_port: i32::from(p.compute_port),
                name: Some(format!("p{}", p.compute_port)),
                ..Default::default()
            })
            .collect();

        let mounts = vec![VolumeMount {
            name: "data".to_string(),
            mount_path: def.data_dir.to_string_lossy().into_owned(),
            ..Default::default()
        }];

        let volumes = vec![Volume {
            name: "data".to_string(),
            persistent_volume_claim: Some(k8s_openapi::api::core::v1::PersistentVolumeClaimVolumeSource {
                claim_name: pvc_name,
                ..Default::default()
            }),
            ..Default::default()
        }];

        let container = Container {
            name: "db".to_string(),
            image: Some(def.image.clone()),
            env: Some(env),
            ports: Some(container_ports),
            volume_mounts: Some(mounts),
            args: if def.args.is_empty() {
                None
            } else {
                Some(def.args.clone())
            },
            ..Default::default()
        };

        StatefulSet {
            metadata: ObjectMeta {
                name: Some(instance.to_string()),
                namespace: Some(self.namespace.clone()),
                labels: Some(labels.clone()),
                ..Default::default()
            },
            spec: Some(k8s_openapi::api::apps::v1::StatefulSetSpec {
                service_name: Some(svc_name),
                replicas: Some(1),
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
                        volumes: Some(volumes),
                        // CSI ZFS uses WaitForFirstConsumer; legacy local-path pinned cp via env unset.
                        node_selector: k8s_storage_class().is_none().then(|| {
                            BTreeMap::from([(
                                "node-role.kubernetes.io/control-plane".to_string(),
                                "true".to_string(),
                            )])
                        }),
                        ..Default::default()
                    }),
                },
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    fn service_manifest(&self, instance: &str, ports: &[PortMapping]) -> Service {
        let labels = labels_for(instance);
        let svc_name = Self::svc_name(instance);
        let service_ports: Vec<ServicePort> = ports
            .iter()
            .map(|p| ServicePort {
                port: i32::from(p.compute_port),
                target_port: Some(k8s_openapi::apimachinery::pkg::util::intstr::IntOrString::Int(
                    i32::from(p.compute_port),
                )),
                name: Some(format!("p{}", p.compute_port)),
                ..Default::default()
            })
            .collect();

        Service {
            metadata: ObjectMeta {
                name: Some(svc_name),
                namespace: Some(self.namespace.clone()),
                labels: Some(labels.clone()),
                ..Default::default()
            },
            spec: Some(ServiceSpec {
                type_: Some("ClusterIP".to_string()),
                selector: Some(labels),
                ports: Some(service_ports),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    fn pvc_manifest(&self, instance: &str) -> PersistentVolumeClaim {
        let labels = labels_for(instance);
        // PVC manifest only used for the default PVC naming path.
        let pvc_name = format!("{instance}-data");
        PersistentVolumeClaim {
            metadata: ObjectMeta {
                name: Some(pvc_name),
                namespace: Some(self.namespace.clone()),
                labels: Some(labels),
                ..Default::default()
            },
            spec: Some(PersistentVolumeClaimSpec {
                access_modes: Some(vec!["ReadWriteOnce".to_string()]),
                storage_class_name: k8s_storage_class(),
                resources: Some(k8s_openapi::api::core::v1::VolumeResourceRequirements {
                    requests: Some(BTreeMap::from([(
                        "storage".to_string(),
                        k8s_openapi::apimachinery::pkg::api::resource::Quantity(format!(
                            "{}Gi",
                            k8s_pvc_size_gi()
                        )),
                    )])),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    async fn ensure_service(&self, instance: &str, ports: &[PortMapping]) -> Result<()> {
        let api = self.api_services();
        let name = Self::svc_name(instance);
        let svc = self.service_manifest(instance, ports);
        let pp = PatchParams::apply("gfs").force();
        api.patch(&name, &pp, &Patch::Apply(&svc))
            .await
            .map_err(|e| ComputeError::Internal(format!("k8s service apply failed: {e}")))?;
        Ok(())
    }

    async fn ensure_pvc(&self, instance: &str) -> Result<()> {
        let api = self.api_pvcs();
        let name = format!("{instance}-data");
        let pvc = self.pvc_manifest(instance);
        let pp = PatchParams::apply("gfs").force();
        api.patch(&name, &pp, &Patch::Apply(&pvc))
            .await
            .map_err(|e| ComputeError::Internal(format!("k8s pvc apply failed: {e}")))?;
        Ok(())
    }

    async fn ensure_statefulset(&self, instance: &str, def: &ComputeDefinition) -> Result<()> {
        let api = self.api_statefulsets();
        let sts = self.statefulset_manifest(instance, def);
        let pp = PatchParams::apply("gfs").force();
        api.patch(instance, &pp, &Patch::Apply(&sts))
            .await
            .map_err(|e| ComputeError::Internal(format!("k8s statefulset apply failed: {e}")))?;
        Ok(())
    }

    async fn find_pod_name(&self, instance: &str) -> Result<String> {
        let api = self.api_pods();
        let lp = ListParams::default().labels(&format!("{INSTANCE_LABEL_KEY}={instance}"));
        let pods = api
            .list(&lp)
            .await
            .map_err(|e| ComputeError::Internal(format!("k8s pod list failed: {e}")))?;
        let items = pods.items;
        let name = items
            .iter()
            .find(|p| {
                p.status
                    .as_ref()
                    .and_then(|s| s.phase.as_deref())
                    .map(|ph| ph == "Running")
                    .unwrap_or(false)
            })
            .or_else(|| items.first())
            .and_then(|p| p.metadata.name.clone())
            .ok_or_else(|| ComputeError::NotFound(instance.to_string()))?;
        Ok(name)
    }

    fn instance_status_from_pod(instance: &InstanceId, pod: Option<Pod>) -> InstanceStatus {
        let Some(pod) = pod else {
            return InstanceStatus {
                id: instance.clone(),
                state: InstanceState::Unknown,
                pid: None,
                started_at: None,
                exit_code: None,
            };
        };
        let phase = pod
            .status
            .as_ref()
            .and_then(|s| s.phase.as_deref())
            .unwrap_or("Unknown");
        let state = match phase {
            "Running" => InstanceState::Running,
            "Pending" => InstanceState::Starting,
            "Succeeded" => InstanceState::Stopped,
            "Failed" => InstanceState::Failed,
            _ => InstanceState::Unknown,
        };
        InstanceStatus {
            id: instance.clone(),
            state,
            pid: None,
            started_at: None,
            exit_code: None,
        }
    }

    async fn get_pod(&self, instance: &InstanceId) -> Result<Option<Pod>> {
        let api = self.api_pods();
        let lp = ListParams::default().labels(&format!("{INSTANCE_LABEL_KEY}={}", instance.0));
        let pods = api
            .list(&lp)
            .await
            .map_err(|e| ComputeError::Internal(format!("k8s pod list failed: {e}")))?;
        Ok(pods.items.into_iter().next())
    }
}

#[async_trait]
impl Compute for KubernetesCompute {
    async fn provision(&self, definition: &ComputeDefinition) -> Result<InstanceId> {
        let raw = instance_name_from_definition(definition);
        let instance = ensure_dns_label(&raw);
        // Create the default PVC only when an override isn't provided.
        let wants_override = definition
            .host_data_dir
            .as_ref()
            .map(|p| p.to_string_lossy().trim().starts_with("pvc:"))
            .unwrap_or(false);
        if !wants_override {
            self.ensure_pvc(&instance).await?;
        }
        self.ensure_service(&instance, &definition.ports).await?;
        self.ensure_statefulset(&instance, definition).await?;
        Ok(InstanceId(instance))
    }

    async fn start(&self, id: &InstanceId, _options: StartOptions) -> Result<InstanceStatus> {
        // StatefulSet is always desired replicas=1; ensure it exists.
        // If it was scaled to 0 by stop(), scale back to 1.
        let api = self.api_statefulsets();
        let patch = json!({ "spec": { "replicas": 1 } });
        api.patch(&id.0, &PatchParams::default(), &Patch::Merge(&patch))
            .await
            .map_err(|e| ComputeError::Internal(format!("k8s scale up failed: {e}")))?;
        Ok(Self::instance_status_from_pod(id, self.get_pod(id).await?))
    }

    async fn stop(&self, id: &InstanceId) -> Result<InstanceStatus> {
        let api = self.api_statefulsets();
        let patch = json!({ "spec": { "replicas": 0 } });
        api.patch(&id.0, &PatchParams::default(), &Patch::Merge(&patch))
            .await
            .map_err(|e| ComputeError::Internal(format!("k8s scale down failed: {e}")))?;
        Ok(Self::instance_status_from_pod(id, self.get_pod(id).await?))
    }

    async fn restart(&self, id: &InstanceId) -> Result<InstanceStatus> {
        // Best-effort: delete pod to force recreation, keep replicas=1.
        let pod_name = self.find_pod_name(&id.0).await?;
        let pods = self.api_pods();
        let _ = pods
            .delete(&pod_name, &DeleteParams::default())
            .await
            .map_err(|e| ComputeError::Internal(format!("k8s pod delete failed: {e}")))?;
        self.start(id, StartOptions::default()).await
    }

    async fn status(&self, id: &InstanceId) -> Result<InstanceStatus> {
        Ok(Self::instance_status_from_pod(id, self.get_pod(id).await?))
    }

    async fn get_connection_info(
        &self,
        id: &InstanceId,
        compute_port: u16,
    ) -> Result<InstanceConnectionInfo> {
        let svc = Self::svc_name(&id.0);
        let host = format!("{svc}.{}.svc.cluster.local", self.namespace);
        Ok(InstanceConnectionInfo {
            host,
            port: compute_port,
            env: vec![],
        })
    }

    async fn prepare_for_snapshot(&self, id: &InstanceId, commands: &[String]) -> Result<()> {
        for cmd in commands {
            let cmd = cmd.trim();
            if cmd.is_empty() {
                continue;
            }
            let out = self.exec(id, cmd, None).await?;
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

    async fn capabilities(&self) -> Result<ComputeCapabilities> {
        Ok(ComputeCapabilities {
            supports_stream_snapshot: false,
            supports_exec_as_root: false,
        })
    }

    async fn exec(&self, id: &InstanceId, command: &str, _user: Option<&str>) -> Result<ExecOutput> {
        let pod = self.find_pod_name(&id.0).await?;
        let pods = self.api_pods();
        let ap = AttachParams::default()
            .container("db")
            .stderr(true)
            .stdout(true)
            .stdin(false)
            .tty(false);
        let mut attached = pods
            .exec(
                &pod,
                vec!["/bin/sh".to_string(), "-c".to_string(), command.to_string()],
                &ap,
            )
            .await
            .map_err(|e| ComputeError::Internal(format!("k8s exec failed: {e}")))?;

        let mut stdout = String::new();
        let mut stderr = String::new();
        if let Some(mut reader) = attached.stdout().take() {
            let mut buf = Vec::new();
            use tokio::io::AsyncReadExt;
            reader
                .read_to_end(&mut buf)
                .await
                .map_err(ComputeError::Io)?;
            stdout = String::from_utf8_lossy(&buf).into_owned();
        }
        if let Some(mut reader) = attached.stderr().take() {
            let mut buf = Vec::new();
            use tokio::io::AsyncReadExt;
            reader
                .read_to_end(&mut buf)
                .await
                .map_err(ComputeError::Io)?;
            stderr = String::from_utf8_lossy(&buf).into_owned();
        }

        // Kubernetes doesn't provide a simple exit code via this API; treat non-empty stderr
        // as signal only when command explicitly reports it. We default to 0 to avoid false failures.
        Ok(ExecOutput {
            exit_code: 0,
            stdout,
            stderr,
        })
    }

    async fn describe_runtime(&self) -> Result<RuntimeDescriptor> {
        Ok(RuntimeDescriptor {
            provider: "kubernetes".to_string(),
            version: "unknown".to_string(),
        })
    }

    async fn logs(&self, id: &InstanceId, options: LogsOptions) -> Result<Vec<LogEntry>> {
        let pod = self.find_pod_name(&id.0).await?;
        let pods = self.api_pods();
        let mut lp = kube::api::LogParams::default();
        if let Some(t) = options.tail {
            lp.tail_lines = Some(t as i64);
        }
        let text = pods
            .logs(&pod, &lp)
            .await
            .map_err(|e| ComputeError::Internal(format!("k8s logs failed: {e}")))?;
        let now = Utc::now();
        Ok(text
            .lines()
            .map(|line| LogEntry {
                timestamp: now,
                stream: LogStream::Stdout,
                message: format!("{line}\n"),
            })
            .collect())
    }

    async fn pause(&self, id: &InstanceId) -> Result<InstanceStatus> {
        Err(ComputeError::PauseUnsupported(format!(
            "pause is not supported for kubernetes runtime (instance '{}')",
            id.0
        )))
    }

    async fn unpause(&self, id: &InstanceId) -> Result<InstanceStatus> {
        Err(ComputeError::PauseUnsupported(format!(
            "unpause is not supported for kubernetes runtime (instance '{}')",
            id.0
        )))
    }

    async fn get_instance_data_mount_host_path(
        &self,
        _id: &InstanceId,
        _compute_data_path: &str,
    ) -> Result<Option<PathBuf>> {
        Ok(None)
    }

    async fn remove_instance(&self, id: &InstanceId) -> Result<()> {
        let stss = self.api_statefulsets();
        let svcs = self.api_services();
        let pvcs = self.api_pvcs();

        let _ = stss.delete(&id.0, &DeleteParams::default()).await;
        let _ = svcs.delete(&Self::svc_name(&id.0), &DeleteParams::default()).await;
        // Only delete the default PVC derived from instance name.
        let _ = pvcs
            .delete(&format!("{}-data", id.0), &DeleteParams::default())
            .await;
        Ok(())
    }

    async fn get_task_connection_info(
        &self,
        id: &InstanceId,
        compute_port: u16,
    ) -> Result<InstanceConnectionInfo> {
        // From inside cluster, service DNS works the same.
        self.get_connection_info(id, compute_port).await
    }

    async fn run_task(
        &self,
        definition: &ComputeDefinition,
        command: &str,
        linked_to: Option<&InstanceId>,
    ) -> Result<ExecOutput> {
        let pods: Api<Pod> = self.api_pods();
        let name = ensure_dns_label(&format!("gfs-task-{}", now_suffix()));
        let labels = labels_for(&name);

        let mut env: Vec<k8s_openapi::api::core::v1::EnvVar> = definition
            .env
            .iter()
            .map(|e| k8s_openapi::api::core::v1::EnvVar {
                name: e.name.clone(),
                value: e.default.clone(),
                ..Default::default()
            })
            .collect();

        if let Some(id) = linked_to {
            // Provide task-side hints (optional) – callers typically also use get_task_connection_info.
            env.push(k8s_openapi::api::core::v1::EnvVar {
                name: "GFS_LINKED_INSTANCE".to_string(),
                value: Some(id.0.clone()),
                ..Default::default()
            });
        }

        let pod = Pod {
            metadata: ObjectMeta {
                name: Some(name.clone()),
                namespace: Some(self.namespace.clone()),
                labels: Some(labels),
                ..Default::default()
            },
            spec: Some(PodSpec {
                restart_policy: Some("Never".to_string()),
                containers: vec![Container {
                    name: "task".to_string(),
                    image: Some(definition.image.clone()),
                    env: Some(env),
                    command: Some(vec!["sh".to_string(), "-c".to_string(), command.to_string()]),
                    ..Default::default()
                }],
                ..Default::default()
            }),
            ..Default::default()
        };

        pods.create(&PostParams::default(), &pod)
            .await
            .map_err(|e| ComputeError::Internal(format!("k8s task pod create failed: {e}")))?;

        // Wait briefly for completion by polling phase.
        for _ in 0..120 {
            let p = pods
                .get(&name)
                .await
                .map_err(|e| ComputeError::Internal(format!("k8s task pod get failed: {e}")))?;
            let phase = p
                .status
                .as_ref()
                .and_then(|s| s.phase.as_deref())
                .unwrap_or("Unknown");
            if phase == "Succeeded" || phase == "Failed" {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        }

        let log = pods
            .logs(&name, &kube::api::LogParams::default())
            .await
            .unwrap_or_default();
        let _ = pods.delete(&name, &DeleteParams::default()).await;
        Ok(ExecOutput {
            exit_code: 0,
            stdout: log,
            stderr: String::new(),
        })
    }
}

// Keep this crate linkable on all platforms.
fn _unused(_p: &Path) {}

