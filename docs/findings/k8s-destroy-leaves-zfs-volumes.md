# Does `gfs destroy` on Kubernetes remove the database's ZFS data?

**Question.** On the Kubernetes backend with OpenEBS ZFS LocalPV, `gfs destroy`
deletes the StatefulSet, the data PVC and every VolumeSnapshot of that PVC. Is
the ZFS dataset holding the data gone afterwards?

**Answer: not reliably, before this fix.** In 3 of 4 runs of the unfixed
binary, one of the repository's volumes stayed on the node after destroy: no
PVC, no PV, no snapshots, but the `ZFSVolume` record was `Ready`, annotated
`openebs.io/marked-for-deletion: "true"`, and its dataset still held the
database's files (89.8 MB in each of the two measured runs). Nothing deletes
such a volume later. With the fix, 0 of 3 runs left anything; in one of them
the fix completed a delete the driver had dropped.

Measured on 2026-10-09 on a two-node k3s stack. Everything below was run by me.

## Why the volume stays

Two parts of OpenEBS ZFS LocalPV 2.11.1 (`pkg/driver/controller.go`, unchanged
in 2.11.2 and on `develop`):

- `DeleteVolume` on a volume that still has snapshots does not delete it. It
  sets `openebs.io/marked-for-deletion` and returns success (l.570–590), so
  Kubernetes removes the PV.
- `DeleteSnapshot` deletes such a volume only when it counts exactly one
  `ZFSSnapshot` record left for it (l.914–937). Those records are removed
  asynchronously by the node agent after the call returns.

When several snapshots of one volume are deleted close together, each call
still counts the others' records, so none of them sees exactly one and nothing
checks again. `gfs destroy` deletes all of a PVC's VolumeSnapshots back to back
(`delete_snapshots_for_pvc`). A checkout recreates the data PVC under the same
name from a snapshot, so one PVC name spans several volumes, and the earlier
ones are deleted by Kubernetes while their snapshots still exist — they are
exactly the marked volumes this race strands.

The controller log shows the deferred delete working when the deletes are
spread out ("volume … deleted after the deletion of last snapshot", 34 times on
this stack) and not firing for the volumes that stayed.

## The fix

`KubernetesCompute::remove_instance_with_pvcs` (the destroy path of both the
CLI and the node daemon) now:

1. Before deleting anything, records every OpenEBS ZFS volume behind the data
   PVC name: the PV bound now, and the volume named in the `snapshotHandle`
   (`<volume>@<snapshot>`) of each of its VolumeSnapshots.
2. After the existing teardown and snapshot deletion, finishes the deferred
   delete (`KubernetesStorage::finish_deferred_openebs_deletes`): a recorded
   `ZFSVolume` is deleted only when it is marked for deletion, has no deletion
   timestamp, no PV of the same name and no `ZFSSnapshot` labelled with it.
   These are the conditions the driver itself requires, and deleting the record
   is the call the driver makes; its node agent then destroys the dataset. It
   polls for up to 120 s and passes over the set repeatedly, because a clone's
   dataset keeps its origin snapshot, and so its parent volume, alive until the
   clone is gone.

A volume whose PV still exists is never touched (one with reclaim policy
`Retain` is left at once), nor is one that was never marked. Checkout is
unchanged and still preserves the snapshots it restores from. On a cluster
without the OpenEBS ZFS driver no volume is recorded in step 1, so step 2 does
nothing; a missing `ZFSVolume` CRD is also treated as nothing to do. Failures
are logged as warnings and do not fail the destroy.

## Setup

| | |
| --- | --- |
| Cluster | k3s v1.34.6+k3s1, two nodes; ZFS pool on the worker |
| Storage | StorageClass `openebs-zfs-gfs` (zfs.csi.openebs.io, reclaim Delete), VolumeSnapshotClass with deletion policy Delete, `openebs/zfs-driver:2.11.1` |
| Unfixed binary | PR head `71e712a`, sha256 `2e381960ccc4…` |
| Fixed binary | `71e712a` + this change, sha256 `ec00d40040bd…` |
| Compiler | `cargo build --release -p gfs-cli`, rustc 1.93.1 (`01f6ddf75`), aarch64 Linux, same target dir |
| Where gfs ran | on the worker node, as root, with the node's kubeconfig |

The fix's log line ("deleted ZFS volumes the driver left behind") occurs once
in the fixed binary and zero times in the unfixed one.

## The run

Each run builds a repository whose PVC spans three volumes: `init` (postgres
17), a 50 MiB random file in the data directory, four commits, `checkout -b
feat`, another 50 MiB file, a commit, `checkout main`. It then checks the
restore (the first file's hash matches and `feat`'s file is absent), records
the volumes behind the PVC, runs `destroy -y`, and looks again 30 s and 90 s
later. The cluster was shared with other databases during the runs, so every
lookup goes through the run's own instance id, PVC and snapshots.

| Run | Binary | Volumes | Restore | Destroy | Left after 90 s | Fix acted |
| --- | --- | --- | --- | --- | --- | --- |
| prefix-a | unfixed | 3 | ok | 1 s | **1 `ZFSVolume`, dataset 89.8 MB, 0 snapshots, marked** | — |
| prefix-b | unfixed | 3 | ok | 1 s | **1 `ZFSVolume`, dataset 89.8 MB, 0 snapshots, marked** | — |
| fixed-a | fixed | 3 | ok | 5 s | 0 | yes, 1 volume |
| fixed-b | fixed | 3 | ok | 2 s | 0 | no |
| fixed-c | fixed | 3 | ok | 4 s | 0 | no |

Two further unfixed runs: one with the same sequence but no data files left
nothing behind; one with gfs 0.4.0 (4 commits, `checkout -b`, a commit,
`checkout main`, `branch -d`, 14 more commits) left its first volume, 240 MB.

A run on the cluster's `local-path` StorageClass (not OpenEBS) with the fixed
binary: `init`, `destroy -y` exit 0 in 1 s, no ZFS log lines, nothing left.

## Not verified

- The race itself is inferred from the driver source and its logs, not
  provoked deterministically; the unfixed binary leaked in 3 of 4 runs here.
- A cluster where the `ZFSVolume` CRD is absent was not run; that path is read
  from the code.
- A clone chain that needs more than one pass was not observed; the volumes in
  these runs were released in one pass.
- The 120 s bound was never reached. A volume still held at the deadline is
  logged and left, as before.

## Volumes orphaned before the fix

The fix only covers databases destroyed by a fixed binary. A volume stranded
earlier is a `ZFSVolume` that is marked for deletion, has no PV and has no
`ZFSSnapshot`. Listing them:

```sh
for zv in $(kubectl get zfsvolumes -A \
    -o jsonpath='{range .items[?(@.metadata.annotations.openebs\.io/marked-for-deletion=="true")]}{.metadata.namespace}/{.metadata.name}{"\n"}{end}'); do
  ns=${zv%/*}; v=${zv#*/}
  kubectl get pv "$v" >/dev/null 2>&1 && continue
  [ "$(kubectl get zfssnapshots -A -l openebs.io/persistent-volume="$v" -o name | wc -l)" -eq 0 ] || continue
  echo "$ns $v"
done
```

Each line names a volume the driver was asked to delete and would have deleted
itself. Review the list, then delete the record; the node agent destroys the
dataset:

```sh
kubectl -n <ns> delete zfsvolume <volume>
```

Run on this stack, the listing named 22 of its 23 volumes: it skipped the one
that still had a snapshot and named that snapshot's stranded clone. Deleting
the records of the three volumes these runs had stranded removed their
datasets (240 MB, 89.8 MB, 89.8 MB) within the `kubectl delete --wait`.

A volume with a `ZFSSnapshot` left is not on the list. If that snapshot is the
origin of another stranded volume (`zfs list -o name,origin`), reclaiming the
clone first lets the node agent finish the snapshot, and the parent appears on
the next listing.
