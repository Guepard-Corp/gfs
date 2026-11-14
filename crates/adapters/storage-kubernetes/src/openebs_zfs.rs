//! Finishing the volume deletes that OpenEBS ZFS LocalPV defers.
//!
//! When a PV is deleted while its ZFS dataset still has snapshots, the driver
//! does not delete the volume. It annotates the `ZFSVolume` record with
//! `openebs.io/marked-for-deletion` and deletes it later, from `DeleteSnapshot`,
//! but only when that call counts exactly one `ZFSSnapshot` record left for the
//! volume. The node agent removes those records asynchronously, so when several
//! snapshots of one volume are deleted close together the last call still
//! counts the others, nothing checks again, and the volume stays: no PVC, no
//! PV, but the dataset and every byte in it remain on the node. A destroy
//! deletes all of a database's snapshots at once, which is exactly that case.
//!
//! [`KubernetesStorage::finish_deferred_openebs_deletes`] completes the delete
//! under the conditions the driver itself applies — the volume was marked, no
//! PV refers to it, and no snapshot of it remains — by deleting the
//! `ZFSVolume` record, which is the call the driver makes. Its node agent then
//! destroys the dataset.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use gfs_domain::ports::storage::StorageError;
use k8s_openapi::api::core::v1::PersistentVolume;
use kube::api::{Api, DeleteParams, DynamicObject, ListParams};
use kube::core::{ApiResource, GroupVersionKind};

use crate::KubernetesStorage;

/// The CSI driver name OpenEBS ZFS LocalPV registers.
pub const OPENEBS_ZFS_DRIVER: &str = "zfs.csi.openebs.io";

/// Annotation the driver sets on a volume it was asked to delete but could not
/// yet, because snapshots of it remained.
const MARKED_FOR_DELETION: &str = "openebs.io/marked-for-deletion";

/// Label the driver puts on every `ZFSSnapshot` naming the volume it belongs to.
const SNAPSHOT_VOLUME_LABEL: &str = "openebs.io/persistent-volume";

fn zfs_volume_resource() -> ApiResource {
    ApiResource::from_gvk(&GroupVersionKind::gvk("zfs.openebs.io", "v1", "ZFSVolume"))
}

fn zfs_snapshot_resource() -> ApiResource {
    ApiResource::from_gvk(&GroupVersionKind::gvk(
        "zfs.openebs.io",
        "v1",
        "ZFSSnapshot",
    ))
}

/// The volume a CSI snapshot handle belongs to, when the snapshot was made by
/// the OpenEBS ZFS driver. Its handles read `<volume>@<snapshot>`.
pub fn openebs_volume_of_snapshot(driver: Option<&str>, handle: Option<&str>) -> Option<String> {
    if driver != Some(OPENEBS_ZFS_DRIVER) {
        return None;
    }
    let (volume, snapshot) = handle?.split_once('@')?;
    if volume.is_empty() || snapshot.is_empty() {
        return None;
    }
    Some(volume.to_string())
}

/// What is known about one volume when deciding whether to finish its delete.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VolumeState {
    /// The `ZFSVolume` record exists.
    pub exists: bool,
    /// The record already has a deletion timestamp: the node agent is on it.
    pub deleting: bool,
    /// The driver marked it for deletion, i.e. Kubernetes asked for it to go.
    pub marked: bool,
    /// A PV of the same name still exists.
    pub has_pv: bool,
    /// That PV's reclaim policy is `Retain`: someone chose to keep the data.
    pub pv_retained: bool,
    /// `ZFSSnapshot` records still labelled with this volume, including ones
    /// being deleted — their datasets are still children of this one.
    pub snapshots: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// The record is gone: nothing left to do.
    Gone,
    /// Delete the `ZFSVolume` record now.
    Reclaim,
    /// Not yet; may become reclaimable on a later pass.
    Wait(&'static str),
    /// Never touch it.
    Leave(&'static str),
}

/// Decide what to do with one volume. Reclaims only under the conditions the
/// driver itself requires before deleting a volume, so it can never delete
/// anything the driver would not have.
pub fn verdict(s: &VolumeState) -> Verdict {
    if !s.exists {
        return Verdict::Gone;
    }
    if s.deleting {
        return Verdict::Wait("the node agent is already destroying it");
    }
    if s.has_pv {
        if s.pv_retained {
            return Verdict::Leave("its PV has reclaim policy Retain");
        }
        return Verdict::Wait("its PV still exists");
    }
    if !s.marked {
        return Verdict::Leave("it was never marked for deletion, so nothing asked for it to go");
    }
    if s.snapshots > 0 {
        return Verdict::Wait("snapshots of it remain");
    }
    Verdict::Reclaim
}

/// The outcome of [`KubernetesStorage::finish_deferred_openebs_deletes`].
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ReclaimReport {
    /// Volumes whose `ZFSVolume` record this call deleted.
    pub reclaimed: Vec<String>,
    /// Volumes still present when the deadline passed, with the last reason.
    pub pending: BTreeMap<String, &'static str>,
    /// Volumes deliberately left alone, with the reason.
    pub left: BTreeMap<String, &'static str>,
}

fn not_found(e: &kube::Error) -> bool {
    matches!(e, kube::Error::Api(err) if err.code == 404)
}

impl KubernetesStorage {
    /// Every OpenEBS ZFS volume that holds, or held, data for `pvc_name`: the
    /// volume bound to the PVC now, plus the volume behind every snapshot of
    /// it. A checkout recreates the PVC under the same name from a snapshot,
    /// so one PVC name can span several volumes. Must be called before the
    /// PVC and its snapshots are deleted; afterwards nothing links them.
    pub async fn openebs_volumes_for_pvc(
        &self,
        pvc_name: &str,
    ) -> std::result::Result<BTreeSet<String>, StorageError> {
        let mut volumes = BTreeSet::new();
        let pvc_name = pvc_name.trim();
        if pvc_name.is_empty() {
            return Ok(volumes);
        }

        let pvs: Api<PersistentVolume> = Api::all(self.client.clone());
        match self.api_pvcs().get(pvc_name).await {
            Ok(pvc) => {
                if let Some(pv_name) = pvc
                    .spec
                    .and_then(|s| s.volume_name)
                    .filter(|n| !n.is_empty())
                {
                    match pvs.get(&pv_name).await {
                        Ok(pv) => {
                            let driver = pv.spec.and_then(|s| s.csi).map(|c| c.driver);
                            if driver.as_deref() == Some(OPENEBS_ZFS_DRIVER) {
                                volumes.insert(pv_name);
                            }
                        }
                        Err(e) if not_found(&e) => {}
                        Err(e) => {
                            return Err(StorageError::Internal(format!(
                                "get pv '{pv_name}' failed: {e}"
                            )));
                        }
                    }
                }
            }
            Err(e) if not_found(&e) => {}
            Err(e) => {
                return Err(StorageError::Internal(format!(
                    "get pvc '{pvc_name}' failed: {e}"
                )));
            }
        }

        // A cluster without the VolumeSnapshot CRD has no snapshots, so the
        // bound volume is the only one.
        let snapshots = match self
            .api_volume_snapshots()
            .list(&ListParams::default())
            .await
        {
            Ok(list) => list,
            Err(e) if not_found(&e) => return Ok(volumes),
            Err(e) => {
                return Err(StorageError::Internal(format!(
                    "list volumesnapshots failed: {e}"
                )));
            }
        };
        let contents = self.api_volume_snapshot_contents();
        for vs in snapshots {
            let src = vs
                .data
                .get("spec")
                .and_then(|s| s.get("source"))
                .and_then(|s| s.get("persistentVolumeClaimName"))
                .and_then(|v| v.as_str());
            if src != Some(pvc_name) {
                continue;
            }
            let Some(content_name) = vs
                .data
                .get("status")
                .and_then(|s| s.get("boundVolumeSnapshotContentName"))
                .and_then(|v| v.as_str())
            else {
                continue;
            };
            let content = match contents.get(content_name).await {
                Ok(c) => c,
                Err(e) if not_found(&e) => continue,
                Err(e) => {
                    return Err(StorageError::Internal(format!(
                        "get volumesnapshotcontent '{content_name}' failed: {e}"
                    )));
                }
            };
            let driver = content
                .data
                .get("spec")
                .and_then(|s| s.get("driver"))
                .and_then(|v| v.as_str());
            let handle = content
                .data
                .get("status")
                .and_then(|s| s.get("snapshotHandle"))
                .and_then(|v| v.as_str());
            if let Some(volume) = openebs_volume_of_snapshot(driver, handle) {
                volumes.insert(volume);
            }
        }
        Ok(volumes)
    }

    /// Finish the deletes the OpenEBS ZFS driver deferred for `volumes`, see
    /// the module docs. Polls until every volume is gone or left alone, or
    /// until `deadline` passes; deleting one volume can unblock another (a
    /// clone's dataset keeps its origin snapshot, and so its parent, alive),
    /// so it keeps passing over the set. A cluster without the OpenEBS ZFS
    /// CRDs has nothing to finish and returns an empty report.
    pub async fn finish_deferred_openebs_deletes(
        &self,
        volumes: &BTreeSet<String>,
        deadline: Duration,
    ) -> std::result::Result<ReclaimReport, StorageError> {
        let mut report = ReclaimReport::default();
        if volumes.is_empty() {
            return Ok(report);
        }
        let zv_res = zfs_volume_resource();
        let started = tokio::time::Instant::now();
        let mut pending: BTreeSet<String> = volumes.clone();
        loop {
            let mut still: BTreeMap<String, &'static str> = BTreeMap::new();
            for volume in &pending {
                let Some((state, found)) = self.assess_openebs_volume(volume).await? else {
                    // No ZFSVolume CRD: this cluster does not run the driver.
                    return Ok(report);
                };
                match verdict(&state) {
                    Verdict::Gone => {}
                    Verdict::Leave(why) => {
                        report.left.insert(volume.clone(), why);
                    }
                    Verdict::Wait(why) => {
                        still.insert(volume.clone(), why);
                    }
                    Verdict::Reclaim => {
                        let Some(zv) = found else { continue };
                        let ns = zv.metadata.namespace.clone().unwrap_or_default();
                        let api: Api<DynamicObject> =
                            Api::namespaced_with(self.client.clone(), &ns, &zv_res);
                        match api.delete(volume, &DeleteParams::default()).await {
                            Ok(_) => report.reclaimed.push(volume.clone()),
                            Err(e) if not_found(&e) => {}
                            Err(e) => {
                                return Err(StorageError::Internal(format!(
                                    "delete zfsvolume '{ns}/{volume}' failed: {e}"
                                )));
                            }
                        }
                        // Keep watching it until the node agent has destroyed it.
                        still.insert(volume.clone(), "the node agent is already destroying it");
                    }
                }
            }
            if still.is_empty() {
                return Ok(report);
            }
            if started.elapsed() >= deadline {
                report.pending = still;
                return Ok(report);
            }
            pending = still.into_keys().collect();
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    }

    /// The state of one OpenEBS ZFS volume and its `ZFSVolume` record, or
    /// `None` when the cluster has no `ZFSVolume` CRD (no OpenEBS ZFS driver).
    async fn assess_openebs_volume(
        &self,
        volume: &str,
    ) -> std::result::Result<Option<(VolumeState, Option<DynamicObject>)>, StorageError> {
        let zv_all: Api<DynamicObject> = Api::all_with(self.client.clone(), &zfs_volume_resource());
        // The driver's namespace is chosen per install, so find the record by
        // name across all of them.
        let found = match zv_all
            .list(&ListParams::default().fields(&format!("metadata.name={volume}")))
            .await
        {
            Ok(list) => list.items.into_iter().next(),
            Err(e) if not_found(&e) => return Ok(None),
            Err(e) => {
                return Err(StorageError::Internal(format!(
                    "list zfsvolumes for '{volume}' failed: {e}"
                )));
            }
        };
        let (pv, snapshots) = if found.is_some() {
            let pvs: Api<PersistentVolume> = Api::all(self.client.clone());
            let pv = match pvs.get(volume).await {
                Ok(pv) => Some(pv),
                Err(e) if not_found(&e) => None,
                Err(e) => {
                    return Err(StorageError::Internal(format!(
                        "get pv '{volume}' failed: {e}"
                    )));
                }
            };
            let snaps_all: Api<DynamicObject> =
                Api::all_with(self.client.clone(), &zfs_snapshot_resource());
            let snapshots = snaps_all
                .list(&ListParams::default().labels(&format!("{SNAPSHOT_VOLUME_LABEL}={volume}")))
                .await
                .map_err(|e| {
                    StorageError::Internal(format!("list zfssnapshots for '{volume}' failed: {e}"))
                })?
                .items
                .len();
            (pv, snapshots)
        } else {
            (None, 0)
        };
        let state = VolumeState {
            exists: found.is_some(),
            deleting: found
                .as_ref()
                .is_some_and(|z| z.metadata.deletion_timestamp.is_some()),
            marked: found.as_ref().is_some_and(is_marked),
            has_pv: pv.is_some(),
            pv_retained: pv
                .as_ref()
                .and_then(|p| p.spec.as_ref())
                .and_then(|s| s.persistent_volume_reclaim_policy.as_deref())
                == Some("Retain"),
            snapshots,
        };
        Ok(Some((state, found)))
    }

    /// Every OpenEBS ZFS volume in the cluster that the driver was asked to
    /// delete and has not: the candidates for
    /// [`Self::finish_deferred_openebs_deletes`] when nothing narrower is
    /// known, e.g. volumes stranded by a destroy that ran before this fix. A
    /// cluster without the driver has none.
    pub async fn marked_openebs_volumes(
        &self,
    ) -> std::result::Result<BTreeSet<String>, StorageError> {
        let zv_all: Api<DynamicObject> = Api::all_with(self.client.clone(), &zfs_volume_resource());
        let list = match zv_all.list(&ListParams::default()).await {
            Ok(list) => list,
            Err(e) if not_found(&e) => return Ok(BTreeSet::new()),
            Err(e) => {
                return Err(StorageError::Internal(format!(
                    "list zfsvolumes failed: {e}"
                )));
            }
        };
        Ok(list
            .items
            .into_iter()
            .filter(is_marked)
            .filter_map(|z| z.metadata.name)
            .collect())
    }

    /// What [`Self::finish_deferred_openebs_deletes`] would do with each of
    /// `volumes` right now, without changing anything, plus the bytes each
    /// dataset holds when the pool is local to this host.
    pub async fn assess_openebs_volumes(
        &self,
        volumes: &BTreeSet<String>,
    ) -> std::result::Result<Vec<VolumeAssessment>, StorageError> {
        let mut out = Vec::new();
        for volume in volumes {
            let Some((state, _)) = self.assess_openebs_volume(volume).await? else {
                break;
            };
            out.push(VolumeAssessment {
                volume: volume.clone(),
                verdict: verdict(&state),
                used_bytes: crate::zfs_dataset_usage(volume).await.map(|(_, used)| used),
            });
        }
        Ok(out)
    }
}

/// One volume's verdict, as reported by
/// [`KubernetesStorage::assess_openebs_volumes`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VolumeAssessment {
    pub volume: String,
    pub verdict: Verdict,
    /// Bytes the dataset uses, when its pool is on this host.
    pub used_bytes: Option<u64>,
}

fn is_marked(z: &DynamicObject) -> bool {
    z.metadata
        .annotations
        .as_ref()
        .and_then(|a| a.get(MARKED_FOR_DELETION))
        .is_some_and(|v| v == "true")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn marked_orphan() -> VolumeState {
        VolumeState {
            exists: true,
            deleting: false,
            marked: true,
            has_pv: false,
            pv_retained: false,
            snapshots: 0,
        }
    }

    #[test]
    fn a_marked_volume_with_no_pv_and_no_snapshots_is_reclaimed() {
        assert_eq!(verdict(&marked_orphan()), Verdict::Reclaim);
    }

    #[test]
    fn a_volume_whose_pv_still_exists_is_never_reclaimed() {
        let s = VolumeState {
            has_pv: true,
            ..marked_orphan()
        };
        assert!(matches!(verdict(&s), Verdict::Wait(_)));
        let retained = VolumeState {
            has_pv: true,
            pv_retained: true,
            ..marked_orphan()
        };
        assert!(matches!(verdict(&retained), Verdict::Leave(_)));
    }

    #[test]
    fn a_volume_with_any_snapshot_left_is_never_reclaimed() {
        let s = VolumeState {
            snapshots: 1,
            ..marked_orphan()
        };
        assert!(matches!(verdict(&s), Verdict::Wait(_)));
    }

    #[test]
    fn a_volume_nothing_asked_to_delete_is_left_alone() {
        let s = VolumeState {
            marked: false,
            ..marked_orphan()
        };
        assert!(matches!(verdict(&s), Verdict::Leave(_)));
    }

    #[test]
    fn a_volume_already_being_destroyed_or_gone_is_not_deleted_again() {
        let s = VolumeState {
            deleting: true,
            ..marked_orphan()
        };
        assert!(matches!(verdict(&s), Verdict::Wait(_)));
        let s = VolumeState {
            exists: false,
            ..marked_orphan()
        };
        assert_eq!(verdict(&s), Verdict::Gone);
    }

    #[test]
    fn only_openebs_zfs_snapshot_handles_name_a_volume() {
        assert_eq!(
            openebs_volume_of_snapshot(
                Some(OPENEBS_ZFS_DRIVER),
                Some("pvc-5465086a-8de7-4a6c-b44b-e490dbc4d89b@snapshot-f86b4d96")
            ),
            Some("pvc-5465086a-8de7-4a6c-b44b-e490dbc4d89b".to_string())
        );
        assert_eq!(
            openebs_volume_of_snapshot(Some("ebs.csi.aws.com"), Some("pvc-1@snap-1")),
            None
        );
        assert_eq!(openebs_volume_of_snapshot(None, Some("pvc-1@snap-1")), None);
        assert_eq!(
            openebs_volume_of_snapshot(Some(OPENEBS_ZFS_DRIVER), None),
            None
        );
        assert_eq!(
            openebs_volume_of_snapshot(Some(OPENEBS_ZFS_DRIVER), Some("no-at-sign")),
            None
        );
        assert_eq!(
            openebs_volume_of_snapshot(Some(OPENEBS_ZFS_DRIVER), Some("@snap")),
            None
        );
        assert_eq!(
            openebs_volume_of_snapshot(Some(OPENEBS_ZFS_DRIVER), Some("pvc-1@")),
            None
        );
    }

    fn zfs_volume(annotations: &[(&str, &str)]) -> DynamicObject {
        let mut z = DynamicObject::new("pvc-1", &zfs_volume_resource());
        if !annotations.is_empty() {
            z.metadata.annotations = Some(
                annotations
                    .iter()
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect(),
            );
        }
        z
    }

    #[test]
    fn only_a_volume_annotated_marked_true_is_a_reclaim_candidate() {
        assert!(is_marked(&zfs_volume(&[(MARKED_FOR_DELETION, "true")])));
        assert!(!is_marked(&zfs_volume(&[(MARKED_FOR_DELETION, "false")])));
        assert!(!is_marked(&zfs_volume(&[("openebs.io/other", "true")])));
        assert!(!is_marked(&zfs_volume(&[])));
    }
}
