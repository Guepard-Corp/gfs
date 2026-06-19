//! Kubernetes (k3s) adapter for the [`Compute`] port.
//!
//! This runtime manages a single Postgres workspace as Kubernetes resources:
//! - PVC for data
//! - StatefulSet (1 replica) mounting the PVC
//! - Service for connectivity (NodePort when `GFS_K8S_EXPOSE_NODEPORT` is enabled)
//!
//! Notes:
//! - `pause`/`unpause` are not supported (no cgroup freezer equivalent).
//! - `host_data_dir` in `ComputeDefinition` is ignored (docker-specific).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use chrono::Utc;
use gfs_domain::ports::compute::{
    Compute, ComputeCapabilities, ComputeDefinition, ComputeError, ExecOutput,
    InstanceConnectionInfo, InstanceId, InstanceState, InstanceStatus, LogEntry, LogStream,
    LogsOptions, PortMapping, Result, RuntimeDescriptor, StartOptions,
};
use k8s_openapi::api::apps::v1::StatefulSet;
use k8s_openapi::api::core::v1::{
    Container, EnvVarSource, PersistentVolumeClaim, PersistentVolumeClaimSpec, Pod, PodSpec,
    PodTemplateSpec, Secret, SecretKeySelector, Service, ServicePort, ServiceSpec, Volume,
    VolumeMount,
};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{LabelSelector, ObjectMeta};
use kube::api::{AttachParams, DeleteParams, ListParams, Patch, PatchParams, PostParams};
use kube::{Api, Client};
use serde_json::json;

const DEFAULT_NAMESPACE: &str = "gfs";
const DEFAULT_PVC_SIZE_GI: &str = "1";

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

/// Pin Postgres to a worker node (e.g. `guepard-dp-01` on Multipass; AWS DP hostname).
fn k8s_schedule_node_name() -> Option<String> {
    std::env::var("GFS_K8S_NODE_NAME")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// When true (default), Postgres Services are NodePort so `get_connection_info`
/// can return a host:port reachable from outside the VPC (worker public IP/EIP).
fn k8s_expose_nodeport() -> bool {
    match std::env::var("GFS_K8S_EXPOSE_NODEPORT")
        .ok()
        .map(|s| s.trim().to_ascii_lowercase())
        .as_deref()
    {
        Some("0" | "false" | "no") => false,
        Some(_) => true,
        None => true,
    }
}

fn k8s_service_type() -> &'static str {
    if k8s_expose_nodeport() {
        "NodePort"
    } else {
        "ClusterIP"
    }
}

/// Per-instance port from Supabase `deployment_request.port` (via `GFS_INSTANCE_NODE_PORT`).
fn instance_expose_port() -> Option<i32> {
    std::env::var("GFS_INSTANCE_NODE_PORT")
        .ok()
        .and_then(|s| s.trim().parse::<i32>().ok())
        .filter(|p| *p > 0)
}

/// Kubernetes NodePort must be in 30000–32767; hostPort on the pod may use any assigned port.
fn is_valid_k8s_node_port(port: i32) -> bool {
    (30000..=32767).contains(&port)
}

/// hostPort on the pod (console `deployment_request.port` or `GFS_INSTANCE_NODE_PORT`).
fn host_port_from_mapping(mapping: &PortMapping) -> Option<i32> {
    mapping
        .host_port
        .map(i32::from)
        .or_else(instance_expose_port)
}

/// NodePort on the Service — only when explicitly pinned and in 30000–32767.
/// Never use cluster-wide env vars (would collide across instances).
fn service_node_port_from_mapping(mapping: &PortMapping) -> Option<i32> {
    host_port_from_mapping(mapping).filter(|p| is_valid_k8s_node_port(*p))
}
const APP_LABEL_KEY: &str = "app.kubernetes.io/name";
const APP_LABEL_VALUE: &str = "gfs";
const INSTANCE_LABEL_KEY: &str = "gfs.guepard.run/instance";

/// A pod is exec-able only when it is Running and its `Ready` condition is True.
fn pod_is_ready(pod: &Pod) -> bool {
    let running = pod.status.as_ref().and_then(|s| s.phase.as_deref()) == Some("Running");
    let ready = pod
        .status
        .as_ref()
        .and_then(|s| s.conditions.as_ref())
        .map(|cs| cs.iter().any(|c| c.type_ == "Ready" && c.status == "True"))
        .unwrap_or(false);
    running && ready
}

fn now_suffix() -> String {
    // short, dns-safe suffix
    format!("{}", Utc::now().timestamp_millis())
}

/// Extract the terminated exit code of a one-shot task pod's container.
///
/// Returns `None` when no container has reached a terminated state yet (the
/// caller then falls back to the pod phase). Task pods created by [`run_task`]
/// have exactly one container, so the first terminated container's code is
/// authoritative.
fn task_container_exit_code(pod: &Pod) -> Option<i32> {
    pod.status
        .as_ref()?
        .container_statuses
        .as_ref()?
        .iter()
        .find_map(|cs| {
            cs.state
                .as_ref()
                .and_then(|state| state.terminated.as_ref())
                .map(|terminated| terminated.exit_code)
        })
}

async fn fetch_task_pod_logs(pods: &kube::Api<Pod>, name: &str) -> String {
    for previous in [false, true] {
        let mut lp = kube::api::LogParams::default();
        lp.previous = previous;
        if let Ok(logs) = pods.logs(name, &lp).await {
            if !logs.is_empty() {
                return logs;
            }
        }
    }
    String::new()
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

/// Name of the per-instance Secret that is the durable home of the
/// deploy-time database credentials (the admin password).
///
/// The StatefulSet references this Secret via `secretKeyRef`, so a
/// checkout/clone reprovision that rebuilds the pod spec from the provider's
/// *default* definition keeps advertising the real deploy-time password
/// instead of the registry default (see `checkout::reprovision_after_pvc_restore`).
pub fn credentials_secret_name(instance: &str) -> String {
    format!("{}-credentials", instance.trim())
}

/// Credential-bearing env vars. Same name rule the domain deploy path uses to
/// inject the generated password (`InitRepositoryUseCase::deploy_database`):
/// any var whose name contains `PASSWORD`.
fn is_credential_env_name(name: &str) -> bool {
    name.contains("PASSWORD")
}

/// Deploy-time credential values present in a definition (credential env vars
/// with a non-empty value).
fn credential_values_from_definition(def: &ComputeDefinition) -> BTreeMap<String, String> {
    def.env
        .iter()
        .filter(|e| is_credential_env_name(&e.name))
        .filter_map(|e| {
            e.default
                .clone()
                .filter(|v| !v.is_empty())
                .map(|v| (e.name.clone(), v))
        })
        .collect()
}

fn credentials_secret_manifest(
    namespace: &str,
    instance: &str,
    values: &BTreeMap<String, String>,
) -> Secret {
    // Populate `data` directly (not the write-only `stringData`): server-side
    // apply then owns the exact field the read path consumes, with no
    // server-side conversion in between.
    let data: BTreeMap<String, k8s_openapi::ByteString> = values
        .iter()
        .map(|(k, v)| (k.clone(), k8s_openapi::ByteString(v.clone().into_bytes())))
        .collect();
    Secret {
        metadata: ObjectMeta {
            name: Some(credentials_secret_name(instance)),
            namespace: Some(namespace.to_string()),
            labels: Some(labels_for(instance)),
            ..Default::default()
        },
        data: Some(data),
        type_: Some("Opaque".to_string()),
        ..Default::default()
    }
}

/// Decode the UTF-8 entries of a credentials Secret.
fn decode_secret_values(secret: Secret) -> BTreeMap<String, String> {
    secret
        .data
        .unwrap_or_default()
        .into_iter()
        .filter_map(|(k, v)| String::from_utf8(v.0).ok().map(|s| (k, s)))
        .collect()
}

/// Container env for the StatefulSet. Credential vars are routed through the
/// per-instance credentials Secret (`secretKeyRef`) instead of a literal
/// value, so a reprovision built from the provider's default definition
/// cannot change the advertised password. `optional: true` keeps pods of
/// pre-Secret instances schedulable: with the var unset, the engine
/// entrypoint skips it (the restored data dir is already initialised) and
/// the platform reports a blank — not wrong — password, which the control
/// plane preserves.
fn container_env_for(
    instance: &str,
    def: &ComputeDefinition,
) -> Vec<k8s_openapi::api::core::v1::EnvVar> {
    def.env
        .iter()
        .map(|e| {
            if is_credential_env_name(&e.name) {
                k8s_openapi::api::core::v1::EnvVar {
                    name: e.name.clone(),
                    value: None,
                    value_from: Some(EnvVarSource {
                        secret_key_ref: Some(SecretKeySelector {
                            name: credentials_secret_name(instance),
                            key: e.name.clone(),
                            optional: Some(true),
                        }),
                        ..Default::default()
                    }),
                }
            } else {
                k8s_openapi::api::core::v1::EnvVar {
                    name: e.name.clone(),
                    value: e.default.clone(),
                    ..Default::default()
                }
            }
        })
        .collect()
}

/// Overlay `overrides` onto `env`: replace same-name entries, append new ones.
fn overlay_env(env: &mut Vec<(String, String)>, overrides: Vec<(String, String)>) {
    for (name, value) in overrides {
        match env.iter_mut().find(|(k, _)| *k == name) {
            Some(entry) => entry.1 = value,
            None => env.push((name, value)),
        }
    }
}

pub mod checkout;

#[derive(Clone)]
pub struct KubernetesCompute {
    client: Client,
    namespace: String,
}

impl KubernetesCompute {
    pub async fn new(namespace: Option<String>) -> std::result::Result<Self, ComputeError> {
        let client = Client::try_default().await.map_err(|e| {
            ComputeError::NotAvailable(format!("kubernetes client unavailable: {e}"))
        })?;
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

    fn api_secrets(&self) -> Api<Secret> {
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

        let env = container_env_for(instance, def);

        let container_ports: Vec<k8s_openapi::api::core::v1::ContainerPort> = def
            .ports
            .iter()
            .map(|p| {
                let mut cp = k8s_openapi::api::core::v1::ContainerPort {
                    container_port: i32::from(p.compute_port),
                    name: Some(format!("p{}", p.compute_port)),
                    ..Default::default()
                };
                if k8s_expose_nodeport() {
                    cp.host_port = host_port_from_mapping(p);
                }
                cp
            })
            .collect();

        let mounts = vec![VolumeMount {
            name: "data".to_string(),
            mount_path: def.data_dir.to_string_lossy().into_owned(),
            ..Default::default()
        }];

        let volumes = vec![Volume {
            name: "data".to_string(),
            persistent_volume_claim: Some(
                k8s_openapi::api::core::v1::PersistentVolumeClaimVolumeSource {
                    claim_name: pvc_name,
                    ..Default::default()
                },
            ),
            ..Default::default()
        }];

        let container = Container {
            name: "db".to_string(),
            image: Some(def.image.clone()),
            image_pull_policy: Some("IfNotPresent".to_string()),
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
                        // Lazy-clone dblink/pg_dump reach external hosts; k3s CoreDNS
                        // is often broken on agents — use the node's resolver.
                        dns_policy: Some("Default".to_string()),
                        containers: vec![container],
                        volumes: Some(volumes),
                        node_selector: k8s_schedule_node_name()
                            .map(|name| {
                                BTreeMap::from([("kubernetes.io/hostname".to_string(), name)])
                            })
                            .or_else(|| {
                                // Legacy local-path: pin to control-plane when no ZFS SC.
                                k8s_storage_class().is_none().then(|| {
                                    BTreeMap::from([(
                                        "node-role.kubernetes.io/control-plane".to_string(),
                                        "true".to_string(),
                                    )])
                                })
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
            .map(|p| {
                let mut sp = ServicePort {
                    port: i32::from(p.compute_port),
                    target_port: Some(
                        k8s_openapi::apimachinery::pkg::util::intstr::IntOrString::Int(i32::from(
                            p.compute_port,
                        )),
                    ),
                    name: Some(format!("p{}", p.compute_port)),
                    ..Default::default()
                };
                if k8s_expose_nodeport() {
                    sp.node_port = service_node_port_from_mapping(p);
                }
                sp
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
                type_: Some(k8s_service_type().to_string()),
                selector: Some(labels),
                ports: Some(service_ports),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    async fn get_service(&self, instance: &str) -> Result<Service> {
        let api = self.api_services();
        let name = Self::svc_name(instance);
        api.get(&name)
            .await
            .map_err(|e| ComputeError::Internal(format!("k8s service get failed: {e}")))
    }

    fn node_port_from_service(svc: &Service, compute_port: u16) -> Option<u16> {
        let ports = svc.spec.as_ref()?.ports.as_ref()?;
        for p in ports {
            if p.port == i32::from(compute_port)
                && let Some(np) = p.node_port.filter(|n| *n > 0)
            {
                return Some(np as u16);
            }
        }
        None
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

    /// Poll for a pod that is Running AND Ready, preferring the newest and
    /// ignoring Terminating pods. After a checkout reprovision the old pod may
    /// still be Terminating while the new one is Pending; exec'ing into a
    /// not-ready pod fails the WebSocket upgrade with `400 Bad Request`.
    async fn wait_ready_pod_name(&self, instance: &str) -> Result<String> {
        use std::time::{Duration, Instant};
        let api = self.api_pods();
        let lp = ListParams::default().labels(&format!("{INSTANCE_LABEL_KEY}={instance}"));
        let deadline = Instant::now() + Duration::from_secs(120);
        loop {
            let pods = api
                .list(&lp)
                .await
                .map_err(|e| ComputeError::Internal(format!("k8s pod list failed: {e}")))?;
            let mut ready: Vec<&Pod> = pods
                .items
                .iter()
                .filter(|p| p.metadata.deletion_timestamp.is_none() && pod_is_ready(p))
                .collect();
            ready.sort_by(|a, b| {
                a.metadata
                    .creation_timestamp
                    .as_ref()
                    .map(|t| t.0)
                    .cmp(&b.metadata.creation_timestamp.as_ref().map(|t| t.0))
            });
            if let Some(name) = ready.last().and_then(|p| p.metadata.name.clone()) {
                return Ok(name);
            }
            if Instant::now() >= deadline {
                let last_phase = pods
                    .items
                    .first()
                    .and_then(|p| p.status.as_ref())
                    .and_then(|s| s.phase.clone())
                    .unwrap_or_else(|| "<none>".into());
                return Err(ComputeError::Internal(format!(
                    "pod for instance '{instance}' not Ready in time (last phase: {last_phase})"
                )));
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
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

    fn env_from_pod(pod: &Pod) -> Vec<(String, String)> {
        let Some(containers) = pod.spec.as_ref().and_then(|s| s.containers.first()) else {
            return vec![];
        };
        containers
            .env
            .as_ref()
            .map(|vars| {
                vars.iter()
                    .filter_map(|e| e.value.as_ref().map(|v| (e.name.clone(), v.clone())))
                    .collect()
            })
            .unwrap_or_default()
    }

    async fn pod_env_for_instance(&self, id: &InstanceId) -> Vec<(String, String)> {
        let mut env = match self.get_pod(id).await {
            Ok(Some(pod)) => Self::env_from_pod(&pod),
            _ => vec![],
        };
        // The credentials Secret is authoritative for credential vars: pod
        // env literals can predate the Secret (legacy instances), and
        // `secretKeyRef` entries carry no inline value at all.
        overlay_env(&mut env, self.credentials_from_secret(&id.0).await);
        if !env.is_empty() {
            return env;
        }
        if let Ok(Ok(out)) = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            self.exec(id, "printenv POSTGRES_PASSWORD", None),
        )
        .await
        {
            let pw = out.stdout.trim();
            if out.exit_code == 0 && !pw.is_empty() {
                return vec![("POSTGRES_PASSWORD".to_string(), pw.to_string())];
            }
        }
        vec![]
    }

    /// Persist the deploy-time credentials as the per-instance Secret.
    ///
    /// Called from `provision` (fresh deploy) ONLY. Reprovision paths
    /// (`provision_pinned`, used by checkout / clone-seed restore) must never
    /// write this Secret: their definition is the provider default, and
    /// applying it would durably replace the real deploy-time password with
    /// the registry default — the exact defect this Secret exists to prevent.
    async fn apply_credentials_secret(
        &self,
        instance: &str,
        def: &ComputeDefinition,
    ) -> Result<()> {
        let values = credential_values_from_definition(def);
        if values.is_empty() {
            return Ok(());
        }
        let name = credentials_secret_name(instance);
        let secret = credentials_secret_manifest(&self.namespace, instance, &values);
        let pp = PatchParams::apply("gfs").force();
        self.api_secrets()
            .patch(&name, &pp, &Patch::Apply(&secret))
            .await
            .map_err(|e| {
                ComputeError::Internal(format!("k8s credentials secret apply failed: {e}"))
            })?;
        Ok(())
    }

    /// Decoded entries of the per-instance credentials Secret (empty when absent).
    async fn credentials_from_secret(&self, instance: &str) -> Vec<(String, String)> {
        match self
            .api_secrets()
            .get_opt(&credentials_secret_name(instance))
            .await
        {
            Ok(Some(secret)) => decode_secret_values(secret).into_iter().collect(),
            _ => vec![],
        }
    }

    /// Make `target_instance` advertise `source_instance`'s credentials.
    ///
    /// Used by clone seeding: the restored volume carries the SOURCE
    /// instance's auth state, so the data answers to the source's deploy-time
    /// password and the target's Secret must be replaced with the source's.
    /// When the source has no Secret (pre-Secret deploy), the target's own
    /// Secret is deleted instead — its freshly generated password is
    /// guaranteed stale once the volume is swapped, and an absent Secret
    /// yields a blank (control-plane-preserved) report rather than a wrong one.
    pub async fn adopt_credentials_secret(
        &self,
        source_instance: &str,
        target_instance: &str,
    ) -> Result<()> {
        let api = self.api_secrets();
        let source_values = api
            .get_opt(&credentials_secret_name(source_instance))
            .await
            .map_err(|e| ComputeError::Internal(format!("k8s credentials secret get failed: {e}")))?
            .map(decode_secret_values)
            .filter(|values| !values.is_empty());

        let target_name = credentials_secret_name(target_instance);
        match source_values {
            Some(values) => {
                let secret = credentials_secret_manifest(&self.namespace, target_instance, &values);
                let pp = PatchParams::apply("gfs").force();
                api.patch(&target_name, &pp, &Patch::Apply(&secret))
                    .await
                    .map_err(|e| {
                        ComputeError::Internal(format!("k8s credentials secret adopt failed: {e}"))
                    })?;
                Ok(())
            }
            None => {
                tracing::warn!(
                    source_instance,
                    target_instance,
                    "source instance has no credentials secret (pre-Secret deploy); deleting \
                     the target's secret so the adopted volume's password is reported blank, \
                     not wrong"
                );
                match api.delete(&target_name, &DeleteParams::default()).await {
                    Ok(_) => Ok(()),
                    Err(kube::Error::Api(err)) if err.code == 404 => Ok(()),
                    Err(e) => Err(ComputeError::Internal(format!(
                        "k8s credentials secret delete failed: {e}"
                    ))),
                }
            }
        }
    }

    /// k3s-only: provision with a fixed StatefulSet name and optional pinned NodePort (30000–32767).
    pub async fn provision_pinned(
        &self,
        definition: &ComputeDefinition,
        instance_name: &str,
        node_port: Option<u16>,
    ) -> Result<InstanceId> {
        let mut ports = definition.ports.clone();
        if let Some(port) = node_port.filter(|p| is_valid_k8s_node_port(i32::from(*p))) {
            for mapping in &mut ports {
                mapping.host_port = Some(port);
            }
        }
        let mut def = definition.clone();
        def.ports = ports;
        self.provision_with_instance(&def, instance_name).await
    }

    async fn provision_with_instance(
        &self,
        definition: &ComputeDefinition,
        instance_name: &str,
    ) -> Result<InstanceId> {
        let instance = ensure_dns_label(instance_name);
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

    /// Tear down StatefulSet/Service and delete `{instance}-data` plus any extra PVC names.
    pub async fn remove_instance_with_pvcs(
        &self,
        id: &InstanceId,
        extra_pvcs: &[String],
    ) -> Result<()> {
        let stss = self.api_statefulsets();
        let svcs = self.api_services();
        let pvcs = self.api_pvcs();

        let _ = stss.delete(&id.0, &DeleteParams::default()).await;
        let _ = svcs
            .delete(&Self::svc_name(&id.0), &DeleteParams::default())
            .await;

        let mut names = vec![format!("{}-data", id.0)];
        for extra in extra_pvcs {
            let e = extra.trim();
            if !e.is_empty() && !names.iter().any(|n| n == e) {
                names.push(e.to_string());
            }
        }
        for name in names {
            let _ = pvcs.delete(&name, &DeleteParams::default()).await;
        }
        Ok(())
    }
}

#[async_trait]
impl Compute for KubernetesCompute {
    async fn provision(&self, definition: &ComputeDefinition) -> Result<InstanceId> {
        let instance = ensure_dns_label(&instance_name_from_definition(definition));
        // Fresh deploy is the ONLY writer of the credentials Secret. It must
        // exist before the StatefulSet so the first pod resolves the real
        // password while the engine bakes it into the data volume on init.
        self.apply_credentials_secret(&instance, definition).await?;
        self.provision_with_instance(definition, &instance).await
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
        let svc_name = Self::svc_name(&id.0);
        let cluster_host = format!("{svc_name}.{}.svc.cluster.local", self.namespace);

        if k8s_expose_nodeport() {
            let host = std::env::var("GUEPARD_EXTERNAL_HOST")
                .or_else(|_| std::env::var("GFS_K8S_EXTERNAL_HOST"))
                .ok()
                .filter(|h| !h.is_empty())
                .unwrap_or(cluster_host);
            let port = {
                let svc = self.get_service(&id.0).await?;
                if let Some(hp) = instance_expose_port() {
                    hp as u16
                } else {
                    Self::node_port_from_service(&svc, compute_port).ok_or_else(|| {
                        ComputeError::Internal(format!(
                            "NodePort not allocated yet for service {svc_name} port {compute_port}"
                        ))
                    })?
                }
            };
            let env = self.pod_env_for_instance(id).await;
            return Ok(InstanceConnectionInfo { host, port, env });
        }

        let env = self.pod_env_for_instance(id).await;
        Ok(InstanceConnectionInfo {
            host: cluster_host,
            port: compute_port,
            env,
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

    async fn exec(
        &self,
        id: &InstanceId,
        command: &str,
        _user: Option<&str>,
    ) -> Result<ExecOutput> {
        let pod = self.wait_ready_pod_name(&id.0).await?;
        let pods = self.api_pods();
        let ap = AttachParams::default()
            .container("db")
            .stderr(true)
            .stdout(true)
            .stdin(false)
            .tty(false);
        // The kubelet stream can still reject the WebSocket upgrade (400) for a
        // moment after the pod reports Ready; retry transient upgrade failures.
        let mut attached = {
            let mut attempt = 0;
            loop {
                match pods
                    .exec(
                        &pod,
                        vec!["/bin/sh".to_string(), "-c".to_string(), command.to_string()],
                        &ap,
                    )
                    .await
                {
                    Ok(a) => break a,
                    Err(e) if attempt < 5 => {
                        attempt += 1;
                        // Exponential backoff (200ms, 400ms, 800ms, …) instead of a
                        // flat 2s. The upgrade rejection right after a pod reports
                        // Ready is almost always transient and clears in well under
                        // a second, so a short first retry avoids adding a fixed ~2s
                        // tail to the common case while still backing off if the
                        // failure genuinely persists.
                        let backoff = std::time::Duration::from_millis(200u64 << (attempt - 1));
                        tokio::time::sleep(backoff).await;
                        tracing::debug!("k8s exec upgrade retry {attempt}/5 in {backoff:?}: {e}");
                    }
                    Err(e) => {
                        return Err(ComputeError::Internal(format!("k8s exec failed: {e}")));
                    }
                }
            }
        };

        let mut stdout = String::new();
        let mut stderr = String::new();
        if let Some(mut reader) = attached.stdout() {
            let mut buf = Vec::new();
            use tokio::io::AsyncReadExt;
            reader
                .read_to_end(&mut buf)
                .await
                .map_err(ComputeError::Io)?;
            stdout = String::from_utf8_lossy(&buf).into_owned();
        }
        if let Some(mut reader) = attached.stderr() {
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
        // Genuine deletion also retires the credentials Secret. The checkout /
        // clone-seed restore tears down via `remove_instance_with_pvcs`, which
        // deliberately preserves the Secret — it is the only durable copy of
        // the deploy-time password and must survive StatefulSet recreation.
        let _ = self
            .api_secrets()
            .delete(&credentials_secret_name(&id.0), &DeleteParams::default())
            .await;
        self.remove_instance_with_pvcs(id, &[]).await
    }

    async fn get_task_connection_info(
        &self,
        id: &InstanceId,
        compute_port: u16,
    ) -> Result<InstanceConnectionInfo> {
        // Task pods run in-cluster (not via NodePort / external host). Prefer the
        // Service ClusterIP over DNS: on split k3s server/agent nodes, agent pods
        // often cannot resolve *.svc.cluster.local while kube-proxy still routes
        // ClusterIPs correctly.
        let svc_name = Self::svc_name(&id.0);
        let svc = self.get_service(&id.0).await?;
        let host = svc
            .spec
            .and_then(|spec| spec.cluster_ip)
            .filter(|ip| !ip.is_empty() && ip != "None")
            .unwrap_or_else(|| format!("{svc_name}.{}.svc.cluster.local", self.namespace));
        let env = self.pod_env_for_instance(id).await;
        Ok(InstanceConnectionInfo {
            host,
            port: compute_port,
            env,
        })
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
                // Task sidecars pg_dump/psql external hosts; cluster DNS (CoreDNS) is
                // often unreachable on k3s agents — inherit the node's resolver instead.
                dns_policy: Some("Default".to_string()),
                containers: vec![Container {
                    name: "task".to_string(),
                    image: Some(definition.image.clone()),
                    image_pull_policy: Some("IfNotPresent".to_string()),
                    env: Some(env),
                    command: Some(vec![
                        "sh".to_string(),
                        "-c".to_string(),
                        command.to_string(),
                    ]),
                    ..Default::default()
                }],
                node_selector: k8s_schedule_node_name()
                    .map(|name| BTreeMap::from([("kubernetes.io/hostname".to_string(), name)])),
                ..Default::default()
            }),
            ..Default::default()
        };

        pods.create(&PostParams::default(), &pod)
            .await
            .map_err(|e| ComputeError::Internal(format!("k8s task pod create failed: {e}")))?;

        // Wait for completion by polling phase. Track the terminal phase and the
        // container's terminated exit code so callers (schema extraction, export)
        // can distinguish success from failure instead of always seeing exit 0.
        let mut terminal_phase = String::from("Unknown");
        let mut exit_code: Option<i32> = None;
        // Clone bootstrap: wait for local DB (up to 120s) + pg_dump remote + FDW setup.
        for _ in 0..360 {
            let p = pods
                .get(&name)
                .await
                .map_err(|e| ComputeError::Internal(format!("k8s task pod get failed: {e}")))?;
            let phase = p
                .status
                .as_ref()
                .and_then(|s| s.phase.as_deref())
                .unwrap_or("Unknown")
                .to_string();
            if let Some(code) = task_container_exit_code(&p) {
                exit_code = Some(code);
                terminal_phase = if code == 0 {
                    "Succeeded".to_string()
                } else {
                    "Failed".to_string()
                };
                break;
            }
            if phase == "Succeeded" || phase == "Failed" {
                exit_code = task_container_exit_code(&p);
                terminal_phase = phase;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        }

        // Fetch logs (sidecar stdout/stderr). Try current container first; fall back
        // to previous only when the kubelet has already restarted the container.
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        let stdout = fetch_task_pod_logs(&pods, &name).await;
        let _ = pods.delete(&name, &DeleteParams::default()).await;

        // Derive the exit code: prefer the container's terminated state; fall
        // back to the pod phase (Failed → 1, Succeeded/Unknown → 0). A pod that
        // never reached a terminal phase within the poll window is treated as a
        // timeout failure so the caller does not record an empty schema as valid.
        let exit_code = match exit_code {
            Some(code) => code,
            None => match terminal_phase.as_str() {
                "Succeeded" => 0,
                "Failed" => 1,
                _ => {
                    return Err(ComputeError::Internal(format!(
                        "k8s task pod '{name}' did not reach a terminal phase \
                         (last phase: {terminal_phase})"
                    )));
                }
            },
        };

        Ok(ExecOutput {
            exit_code,
            stdout,
            stderr: String::new(),
        })
    }
}

// Keep this crate linkable on all platforms.
fn _unused(_p: &Path) {}

#[cfg(test)]
mod tests {
    use super::*;
    use gfs_domain::ports::compute::EnvVar;
    use k8s_openapi::ByteString;

    fn definition_with_env(env: Vec<EnvVar>) -> ComputeDefinition {
        ComputeDefinition {
            labels: Default::default(),
            image: "postgres:17".into(),
            env,
            ports: vec![],
            data_dir: PathBuf::from("/var/lib/postgresql/data"),
            host_data_dir: None,
            user: None,
            logs_dir: None,
            conf_dir: None,
            args: vec![],
        }
    }

    fn env_var(name: &str, default: Option<&str>) -> EnvVar {
        EnvVar {
            name: name.to_string(),
            default: default.map(str::to_string),
        }
    }

    fn pod_with_container_state(state: Option<k8s_openapi::api::core::v1::ContainerState>) -> Pod {
        use k8s_openapi::api::core::v1::{ContainerStatus, PodStatus};
        Pod {
            status: Some(PodStatus {
                container_statuses: state.map(|s| {
                    vec![ContainerStatus {
                        name: "task".into(),
                        state: Some(s),
                        ..Default::default()
                    }]
                }),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    #[test]
    fn task_exit_code_reads_terminated_state() {
        use k8s_openapi::api::core::v1::{ContainerState, ContainerStateTerminated};
        // A failed task (e.g. pg_dump could not connect) must surface its real
        // non-zero exit code, not a hardcoded 0 that hides the failure.
        let pod = pod_with_container_state(Some(ContainerState {
            terminated: Some(ContainerStateTerminated {
                exit_code: 2,
                ..Default::default()
            }),
            ..Default::default()
        }));
        assert_eq!(task_container_exit_code(&pod), Some(2));
    }

    #[test]
    fn task_exit_code_zero_on_success() {
        use k8s_openapi::api::core::v1::{ContainerState, ContainerStateTerminated};
        let pod = pod_with_container_state(Some(ContainerState {
            terminated: Some(ContainerStateTerminated {
                exit_code: 0,
                ..Default::default()
            }),
            ..Default::default()
        }));
        assert_eq!(task_container_exit_code(&pod), Some(0));
    }

    #[test]
    fn task_exit_code_none_when_not_terminated() {
        use k8s_openapi::api::core::v1::ContainerState;
        // Still running / no status yet → None, so the caller falls back to phase.
        assert_eq!(
            task_container_exit_code(&pod_with_container_state(Some(ContainerState::default()))),
            None
        );
        assert_eq!(
            task_container_exit_code(&pod_with_container_state(None)),
            None
        );
        assert_eq!(task_container_exit_code(&Pod::default()), None);
    }

    #[test]
    fn credentials_secret_name_is_instance_scoped() {
        assert_eq!(credentials_secret_name("gfs-pg-1"), "gfs-pg-1-credentials");
        assert_eq!(
            credentials_secret_name(" gfs-pg-1 "),
            "gfs-pg-1-credentials"
        );
    }

    #[test]
    fn credential_values_pick_password_vars_with_values_only() {
        let def = definition_with_env(vec![
            env_var("POSTGRES_USER", Some("postgres")),
            env_var("POSTGRES_PASSWORD", Some("s3cret")),
            env_var("MYSQL_ROOT_PASSWORD", Some("")),
            env_var("EMPTY_PASSWORD", None),
            env_var("POSTGRES_DB", Some("postgres")),
        ]);
        let values = credential_values_from_definition(&def);
        assert_eq!(values.len(), 1);
        assert_eq!(
            values.get("POSTGRES_PASSWORD").map(String::as_str),
            Some("s3cret")
        );
    }

    #[test]
    fn container_env_routes_password_through_optional_secret_key_ref() {
        let def = definition_with_env(vec![
            env_var("POSTGRES_USER", Some("postgres")),
            env_var("POSTGRES_PASSWORD", Some("s3cret")),
            env_var("POSTGRES_DB", Some("postgres")),
        ]);
        let env = container_env_for("gfs-pg-1", &def);

        let password = env
            .iter()
            .find(|e| e.name == "POSTGRES_PASSWORD")
            .expect("password env var present");
        // The pod spec must never carry the password as a literal value:
        // a reprovision from the provider default would silently change it.
        assert_eq!(password.value, None);
        let key_ref = password
            .value_from
            .as_ref()
            .and_then(|v| v.secret_key_ref.as_ref())
            .expect("password resolves via secretKeyRef");
        assert_eq!(key_ref.name, "gfs-pg-1-credentials");
        assert_eq!(key_ref.key, "POSTGRES_PASSWORD");
        // optional: a pre-Secret (legacy) instance must still schedule; the
        // unset var yields a blank — not wrong — advertised password.
        assert_eq!(key_ref.optional, Some(true));

        for name in ["POSTGRES_USER", "POSTGRES_DB"] {
            let var = env
                .iter()
                .find(|e| e.name == name)
                .expect("env var present");
            assert_eq!(var.value.as_deref(), Some("postgres"));
            assert!(var.value_from.is_none());
        }
    }

    #[test]
    fn credentials_secret_manifest_carries_deploy_time_values() {
        let values = BTreeMap::from([("POSTGRES_PASSWORD".to_string(), "s3cret".to_string())]);
        let secret = credentials_secret_manifest("gfs", "gfs-pg-1", &values);
        assert_eq!(
            secret.metadata.name.as_deref(),
            Some("gfs-pg-1-credentials")
        );
        assert_eq!(secret.metadata.namespace.as_deref(), Some("gfs"));
        assert_eq!(secret.type_.as_deref(), Some("Opaque"));
        // Write/read symmetry: the manifest populates `data` — the same field
        // the read path (`decode_secret_values`) consumes — not `stringData`.
        assert!(secret.string_data.is_none());
        assert_eq!(
            decode_secret_values(secret)
                .get("POSTGRES_PASSWORD")
                .map(String::as_str),
            Some("s3cret")
        );
    }

    #[test]
    fn decode_secret_values_reads_utf8_data() {
        let secret = Secret {
            data: Some(BTreeMap::from([
                (
                    "POSTGRES_PASSWORD".to_string(),
                    ByteString(b"s3cret".to_vec()),
                ),
                ("BINARY".to_string(), ByteString(vec![0xff, 0xfe])),
            ])),
            ..Default::default()
        };
        let values = decode_secret_values(secret);
        assert_eq!(
            values.get("POSTGRES_PASSWORD").map(String::as_str),
            Some("s3cret")
        );
        assert!(!values.contains_key("BINARY"));
    }

    #[test]
    fn overlay_env_replaces_and_appends() {
        // The Secret value must win over a (possibly lying) pod env literal.
        let mut env = vec![
            ("POSTGRES_USER".to_string(), "postgres".to_string()),
            ("POSTGRES_PASSWORD".to_string(), "postgres".to_string()),
        ];
        overlay_env(
            &mut env,
            vec![
                ("POSTGRES_PASSWORD".to_string(), "s3cret".to_string()),
                ("EXTRA".to_string(), "x".to_string()),
            ],
        );
        assert_eq!(
            env,
            vec![
                ("POSTGRES_USER".to_string(), "postgres".to_string()),
                ("POSTGRES_PASSWORD".to_string(), "s3cret".to_string()),
                ("EXTRA".to_string(), "x".to_string()),
            ]
        );
    }

    #[test]
    fn overlay_env_with_no_overrides_keeps_env() {
        let mut env = vec![("POSTGRES_PASSWORD".to_string(), "literal".to_string())];
        overlay_env(&mut env, vec![]);
        assert_eq!(
            env,
            vec![("POSTGRES_PASSWORD".to_string(), "literal".to_string())]
        );
    }
}
