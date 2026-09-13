//! The public surface the data-platform daemon calls, pinned by compilation.
//!
//! `data-platform-v3` depends on these crates by path and calls into them
//! directly rather than through the CLI, so a signature change here breaks a
//! build in another repository — where it surfaces as someone else's red CI,
//! long after the change that caused it.
//!
//! These functions are never run. They exist so that changing one of those
//! signatures fails HERE, with a message naming the daemon, instead of there.
//! The call shapes are copied from:
//!
//! - `crates/data-plane/cli/src/runner.rs` (restore, remove_instance_with_pvcs)
//! - `crates/data-plane/cli/src/main.rs`   (restore, stable_data_pvc)
//!
//! If a change genuinely needs to alter one of these, update this file and the
//! daemon together — deliberately, rather than by discovering it downstream.

use std::path::Path;
use std::sync::Arc;

use gfs_compute_kubernetes::KubernetesCompute;
use gfs_compute_kubernetes::checkout::{restore_database_volume_from_snapshot, stable_data_pvc};
use gfs_domain::ports::compute::InstanceId;
use gfs_domain::ports::database_provider::InMemoryDatabaseProviderRegistry;
use gfs_domain::ports::repository::Repository;
use gfs_domain::ports::storage::{StoragePort, VolumeId};
use gfs_storage_kubernetes::KubernetesStorage;

/// `runner.rs` and `main.rs`: the checkout restore, with a concrete registry.
#[allow(dead_code)]
async fn daemon_restores_a_volume(
    storage: &KubernetesStorage,
    compute: &KubernetesCompute,
    registry: Arc<InMemoryDatabaseProviderRegistry>,
    repository: Arc<dyn Repository>,
    repo_path: &Path,
    snapshot_hash: &str,
) {
    let _ = restore_database_volume_from_snapshot(
        storage,
        compute,
        registry,
        repository,
        repo_path,
        snapshot_hash,
    )
    .await;
}

/// `runner.rs`: a full destroy, passing NO extra PVCs.
///
/// The empty slice is the reason `remove_instance_with_pvcs` reclaims by label
/// as well as by derived name: the daemon does not know the names of an
/// instance's per-branch volumes, so a name-only sweep would leak them.
#[allow(dead_code)]
async fn daemon_destroys_an_instance(compute: &KubernetesCompute) {
    let _ = compute
        .remove_instance_with_pvcs(&InstanceId("gfs-pg-1".to_string()), &[])
        .await;
}

/// `main.rs`: disk usage, by deriving the PVC name from the container name.
///
/// NOTE this derivation is only the STARTING volume now that each branch owns
/// one. The daemon should read `config.mount_point`, which is authoritative.
/// Kept compiling because removing the function would break their build. The
/// behavioural consequence — the daemon reporting the starting volume's usage for
/// every branch, so a branch with its own volume is measured as whatever the
/// first one holds — is tracked outside this repository.
#[allow(dead_code)]
async fn daemon_measures_disk(storage: &Arc<dyn StoragePort>, container_name: &str) -> u64 {
    let pvc = stable_data_pvc(container_name);
    storage
        .status(&VolumeId(pvc))
        .await
        .map(|status| status.used_bytes)
        .unwrap_or(0)
}

#[test]
fn the_daemon_facing_api_still_has_the_shape_the_daemon_calls() {
    // The assertion is that this file compiled. Keep one runtime check so the
    // test is not silently optimised into nothing.
    assert_eq!(stable_data_pvc("gfs-pg-1"), "gfs-pg-1-data");
}
