//! Kubernetes (k3s) storage adapter for the [`StoragePort`] port.
//!
//! Interprets:
//! - `VolumeId.0` as a **PVC name** in the configured namespace
//! - `SnapshotId.0` as a **VolumeSnapshot name** in the configured namespace
//!
//! Requires the VolumeSnapshot CRDs and a compatible CSI driver.

use std::collections::BTreeMap;
use std::path::Path;

use async_trait::async_trait;
use chrono::Utc;
use gfs_domain::ports::storage::{
    CloneOptions, MountStatus, Quota, Result, Snapshot, SnapshotId, SnapshotOptions, StorageError,
    StoragePort, VolumeId, VolumeStatus,
};
use k8s_openapi::api::core::v1::{PersistentVolumeClaim, PersistentVolumeClaimSpec};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use kube::Client;
use kube::api::{Api, DeleteParams, DynamicObject, Patch, PatchParams};
use kube::core::{ApiResource, GroupVersionKind};
use serde_json::json;

const DEFAULT_NAMESPACE: &str = "gfs";
const DEFAULT_SNAPSHOT_CLASS: &str = "openebs-zfs-gfs-snapclass";
const DEFAULT_PVC_SIZE_GI: &str = "1";

fn k8s_snapshot_class() -> String {
    std::env::var("GFS_K8S_SNAPSHOT_CLASS")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| DEFAULT_SNAPSHOT_CLASS.to_string())
}

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

fn now_suffix() -> String {
    format!("{}", Utc::now().timestamp_millis())
}

fn volume_snapshot_gvk() -> GroupVersionKind {
    GroupVersionKind::gvk("snapshot.storage.k8s.io", "v1", "VolumeSnapshot")
}

fn volume_snapshot_content_gvk() -> GroupVersionKind {
    GroupVersionKind::gvk("snapshot.storage.k8s.io", "v1", "VolumeSnapshotContent")
}

fn snapshot_hash_from_label(label: Option<&str>) -> Option<String> {
    // commit use case passes label as a destination path:
    //   .../.gfs/snapshots/<2>/<62>
    // Reconstruct the 64-char hash from the last two path segments.
    let label = label?;
    let parts: Vec<&str> = label.trim_end_matches('/').split('/').collect();
    if parts.len() < 2 {
        return None;
    }
    let h = format!("{}{}", parts[parts.len() - 2], parts[parts.len() - 1]);
    if h.len() == 64 && h.chars().all(|c| c.is_ascii_hexdigit()) {
        Some(h.to_ascii_lowercase())
    } else {
        None
    }
}

fn volumesnapshot_name_for_hash(hash: &str) -> String {
    // DNS label <= 63. Keep stable + deterministic.
    // Use first 32 chars to keep name short but collision-resistant.
    format!("gfs-snap-{}", &hash[..32.min(hash.len())])
}

#[derive(Clone)]
pub struct KubernetesStorage {
    client: Client,
    namespace: String,
}

impl KubernetesStorage {
    pub async fn new(namespace: Option<String>) -> std::result::Result<Self, StorageError> {
        let client = Client::try_default()
            .await
            .map_err(|e| StorageError::Internal(format!("kubernetes client unavailable: {e}")))?;
        Ok(Self {
            client,
            namespace: namespace.unwrap_or_else(|| DEFAULT_NAMESPACE.to_string()),
        })
    }

    fn api_pvcs(&self) -> Api<PersistentVolumeClaim> {
        Api::namespaced(self.client.clone(), &self.namespace)
    }

    fn api_volume_snapshots(&self) -> Api<DynamicObject> {
        let gvk = volume_snapshot_gvk();
        let ar = ApiResource::from_gvk(&gvk);
        Api::namespaced_with(self.client.clone(), &self.namespace, &ar)
    }

    /// VolumeSnapshotContents are cluster-scoped; used to read `snapshotHandle`.
    fn api_volume_snapshot_contents(&self) -> Api<DynamicObject> {
        let gvk = volume_snapshot_content_gvk();
        let ar = ApiResource::from_gvk(&gvk);
        Api::all_with(self.client.clone(), &ar)
    }

    /// Delete a PVC if it exists (best-effort; waits for removal).
    pub async fn delete_pvc(&self, name: &str) -> std::result::Result<(), StorageError> {
        let pvcs = self.api_pvcs();
        let name = name.trim();
        if name.is_empty() {
            return Ok(());
        }
        match pvcs.delete(name, &DeleteParams::default()).await {
            Ok(_) => {}
            Err(kube::Error::Api(err)) if err.code == 404 => return Ok(()),
            Err(e) => {
                return Err(StorageError::Internal(format!("delete pvc failed: {e}")));
            }
        }
        for _ in 0..120 {
            if pvcs.get(name).await.is_err() {
                return Ok(());
            }
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        }
        Err(StorageError::Internal(format!(
            "pvc '{name}' still exists after delete"
        )))
    }

    /// Wait until PVC phase is Bound (after restore from VolumeSnapshot).
    pub async fn wait_pvc_bound(&self, name: &str) -> std::result::Result<(), StorageError> {
        let pvcs = self.api_pvcs();
        for _ in 0..240 {
            let pvc = pvcs
                .get(name)
                .await
                .map_err(|_| StorageError::NotFound(name.to_string()))?;
            let phase = pvc
                .status
                .as_ref()
                .and_then(|s| s.phase.as_deref())
                .unwrap_or("");
            if phase == "Bound" {
                return Ok(());
            }
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        }
        Err(StorageError::Internal(format!(
            "pvc '{name}' did not reach Bound in time"
        )))
    }

    /// Source PVC recorded in a VolumeSnapshot's `spec.source.persistentVolumeClaimName`.
    ///
    /// Identifies which instance's volume (and therefore whose auth state) a
    /// snapshot was taken from — the restore path uses it to keep the
    /// advertised credentials truthful when seeding a clone from another
    /// instance's snapshot.
    pub async fn snapshot_source_pvc(
        &self,
        name: &str,
    ) -> std::result::Result<Option<String>, StorageError> {
        let api = self.api_volume_snapshots();
        let vs = api
            .get(name)
            .await
            .map_err(|e| StorageError::Internal(format!("get volumesnapshot failed: {e}")))?;
        Ok(vs
            .data
            .get("spec")
            .and_then(|s| s.get("source"))
            .and_then(|s| s.get("persistentVolumeClaimName"))
            .and_then(|v| v.as_str())
            .map(str::to_string))
    }

    pub async fn wait_snapshot_ready(&self, name: &str) -> std::result::Result<(), StorageError> {
        let api = self.api_volume_snapshots();
        let contents = self.api_volume_snapshot_contents();
        // `readyToUse` is the external-snapshotter's *final* status flip; on OpenEBS
        // ZFS it lags the actual snapshot by ~1.6s (measured: snapshotHandle ~0.7s,
        // readyToUse ~2.3s). Return as soon as the bound VolumeSnapshotContent reports
        // a `snapshotHandle` — that means the CSI driver already created the
        // copy-on-write snapshot, so the data is captured and a later
        // clone-from-snapshot is valid. The clone path waits for `readyToUse` via the
        // external-provisioner (the clone PVC stays Pending until then), so this only
        // moves the reconcile lag off the commit hot path into the rare checkout path.
        // ZFS VolumeSnapshots on dev k3s can take >60s under load.
        //
        // Poll at 100ms: the snapshotHandle lands at ~0.7s, so a coarser interval
        // adds avg ~half-interval of dead wait after the data is already captured.
        // A `watch` would be marginally tighter but the granularity is a small
        // fraction of the snapshot cost, so the simpler bounded poll is kept.
        for _ in 0..1800 {
            let vs = api
                .get(name)
                .await
                .map_err(|e| StorageError::Internal(format!("get volumesnapshot failed: {e}")))?;
            let status = vs.data.get("status");
            let ready = status
                .and_then(|s| s.get("readyToUse"))
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            if ready {
                return Ok(());
            }
            // Fast path: the snapshot's data is captured once its content has a handle.
            if let Some(content_name) = status
                .and_then(|s| s.get("boundVolumeSnapshotContentName"))
                .and_then(|v| v.as_str())
                && let Ok(content) = contents.get(content_name).await
                && content
                    .data
                    .get("status")
                    .and_then(|s| s.get("snapshotHandle"))
                    .and_then(|v| v.as_str())
                    .is_some_and(|h| !h.is_empty())
            {
                return Ok(());
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        Err(StorageError::Internal(format!(
            "volumesnapshot '{name}' was not captured in time"
        )))
    }
}

#[async_trait]
impl StoragePort for KubernetesStorage {
    async fn mount(&self, _id: &VolumeId, _mount_point: &Path) -> Result<()> {
        // Not applicable: PVCs are mounted by Kubernetes workloads, not the host process.
        Ok(())
    }

    async fn unmount(&self, _id: &VolumeId) -> Result<()> {
        Ok(())
    }

    async fn snapshot(&self, id: &VolumeId, options: SnapshotOptions) -> Result<Snapshot> {
        let pvc_name = id.0.trim();
        if pvc_name.is_empty() {
            return Err(StorageError::Internal("empty pvc name".into()));
        }

        // Ensure PVC exists (clear NotFound early).
        let pvcs = self.api_pvcs();
        pvcs.get(pvc_name)
            .await
            .map_err(|_| StorageError::NotFound(pvc_name.to_string()))?;

        let api = self.api_volume_snapshots();
        let snap_hash = snapshot_hash_from_label(options.label.as_deref());
        let snap_name = snap_hash
            .as_deref()
            .map(volumesnapshot_name_for_hash)
            .unwrap_or_else(|| format!("gfs-snap-{}", now_suffix()));

        // options.label is a filesystem path in file storage; in k8s we keep it as metadata only.
        let manifest = json!({
          "apiVersion": "snapshot.storage.k8s.io/v1",
          "kind": "VolumeSnapshot",
          "metadata": {
            "name": snap_name,
            "namespace": self.namespace,
            "labels": {
              "app.kubernetes.io/name": "gfs",
            },
            "annotations": {
              "gfs.guepard.run/label": options.label,
              "gfs.guepard.run/snapshot_hash": snap_hash
            }
          },
          "spec": {
            "volumeSnapshotClassName": k8s_snapshot_class(),
            "source": { "persistentVolumeClaimName": pvc_name }
          }
        });

        api.patch(
            &snap_name,
            &PatchParams::apply("gfs").force(),
            &Patch::Apply(&manifest),
        )
        .await
        .map_err(|e| {
            StorageError::Internal(format!(
                "failed to create VolumeSnapshot (CRDs installed?): {e}"
            ))
        })?;

        self.wait_snapshot_ready(&snap_name).await?;

        Ok(Snapshot {
            id: SnapshotId(snap_name),
            volume_id: id.clone(),
            created_at: Utc::now(),
            size_bytes: 0,
            label: options.label,
        })
    }

    async fn clone(
        &self,
        _source: &VolumeId,
        target_id: VolumeId,
        options: CloneOptions,
    ) -> Result<VolumeStatus> {
        let pvcs = self.api_pvcs();
        let target = target_id.0.trim().to_string();
        if target.is_empty() {
            return Err(StorageError::Internal("empty target pvc name".into()));
        }

        let mut spec = PersistentVolumeClaimSpec {
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
        };

        if let Some(from) = options.from_snapshot {
            // PVC from VolumeSnapshot
            spec.data_source = Some(k8s_openapi::api::core::v1::TypedLocalObjectReference {
                api_group: Some("snapshot.storage.k8s.io".to_string()),
                kind: "VolumeSnapshot".to_string(),
                name: from.0,
            });
        } else {
            return Err(StorageError::Internal(
                "clone without from_snapshot is not supported for kubernetes storage".into(),
            ));
        }

        let pvc = PersistentVolumeClaim {
            metadata: ObjectMeta {
                name: Some(target.clone()),
                namespace: Some(self.namespace.clone()),
                labels: Some(BTreeMap::from([(
                    "app.kubernetes.io/name".to_string(),
                    "gfs".to_string(),
                )])),
                ..Default::default()
            },
            spec: Some(spec),
            ..Default::default()
        };

        // Apply-create (idempotent)
        pvcs.patch(
            &target,
            &PatchParams::apply("gfs").force(),
            &Patch::Apply(&pvc),
        )
        .await
        .map_err(|e| StorageError::Internal(format!("failed to create PVC from snapshot: {e}")))?;

        Ok(VolumeStatus {
            id: VolumeId(target),
            mount_point: None,
            status: MountStatus::Mounted,
            size_bytes: 0,
            used_bytes: 0,
        })
    }

    async fn status(&self, id: &VolumeId) -> Result<VolumeStatus> {
        let pvcs = self.api_pvcs();
        let pvc_name = id.0.trim();
        let pvc = pvcs
            .get(pvc_name)
            .await
            .map_err(|_| StorageError::NotFound(pvc_name.to_string()))?;
        let phase = pvc
            .status
            .as_ref()
            .and_then(|s| s.phase.as_deref())
            .unwrap_or("Unknown");
        let status = if phase == "Bound" {
            MountStatus::Mounted
        } else {
            MountStatus::Unknown
        };
        Ok(VolumeStatus {
            id: id.clone(),
            mount_point: None,
            status,
            size_bytes: 0,
            used_bytes: 0,
        })
    }

    async fn quota(&self, id: &VolumeId) -> Result<Quota> {
        // No reliable per-PVC quota information from core APIs without querying metrics.
        Ok(Quota {
            volume_id: id.clone(),
            limit_bytes: 0,
            used_bytes: 0,
            free_bytes: 0,
        })
    }

    async fn finalize_snapshot(&self, _dest: &Path) -> Result<()> {
        // Not applicable to CSI snapshots.
        Ok(())
    }
}
