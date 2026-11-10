# Does `gfs destroy` on Kubernetes remove the database's ZFS data?

**Question.** On the Kubernetes backend with OpenEBS ZFS LocalPV, `gfs destroy`
deletes the StatefulSet, the data PVC and every VolumeSnapshot of that PVC. Is
the ZFS dataset holding the data gone afterwards?

**Answer: not reliably, before this fix.** In 5 of 7 runs of the unfixed
binary, one of the repository's volumes stayed on the node after destroy: no
PVC, no PV, no snapshots, but the `ZFSVolume` record was `Ready`, annotated
`openebs.io/marked-for-deletion: "true"`, and its dataset still held the
database's files (89.8 MB each time). Nothing deletes such a volume later.
With the fix, 0 of 5 runs left anything; in two of them the fix completed a
delete the driver had dropped.

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
| Fixed binary | `71e712a` + this change: sha256 `ec00d40040bd…` (first version), `acade3aaf187…` (shared with `storage reclaim`), `11860dd3809b…` (final, every source identical to the branch head) |
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
| fixed2-a | fixed (`acade3aa`) | 3 | ok | — | 0 | yes, 1 volume |
| fixed3-a | fixed (final) | 3 | ok | 4 s | 0 | no |
| orphan-a | unfixed | 3 | ok | 0 s | 0 | — |
| orphan-b | unfixed | 3 | ok | 1 s | **1 `ZFSVolume`, dataset 89.8 MB, 0 snapshots, marked** | — |
| orphan-c | unfixed | 3 | ok | 0 s | 0 | — |
| orphan-d | unfixed | 3 | ok | 0 s | **1 `ZFSVolume`, dataset 89.8 MB, 0 snapshots, marked** | — |
| prompt-a | unfixed | 3 | ok | — | **1 `ZFSVolume`, dataset 89.8 MB, 0 snapshots, marked** | — |

The `orphan` and `prompt` runs stranded volumes on purpose, to reclaim them
(below). If the fix did nothing, five clean runs in a row at the unfixed leak
rate (5 of 7) would happen about 0.2% of the time; the two runs where the fix acted are the
direct evidence.

Two further unfixed runs: one with the same sequence but no data files left
nothing behind; one with gfs 0.4.0 (4 commits, `checkout -b`, a commit,
`checkout main`, `branch -d`, 14 more commits) left its first volume, 240 MB.

A run on the cluster's `local-path` StorageClass (not OpenEBS) with the fixed
binary: `init`, `destroy -y` exit 0 in 1 s, no ZFS log lines, nothing left.

**A cluster without OpenEBS.** A kind cluster (Kubernetes v1.37, `local-path`,
no CRDs at all: neither `ZFSVolume` nor `VolumeSnapshot`), gfs built on macOS
at the PR head and at this branch. `init` then `destroy -y`: exit 0 with both,
no StatefulSet, PVC, Service, Secret or PV left. Both log the existing warning
that snapshot cleanup could not list VolumeSnapshots. The first build of this
change also logged "could not list the ZFS volumes", because it treated the
missing CRD as an error rather than as no snapshots; that is fixed, and the
re-run with the fixed build logs only the existing warning. On the same
cluster, `gfs storage reclaim` prints "No OpenEBS ZFS volume is waiting to be
deleted." (exit 0), `--json` prints an empty list, and `--yes`, with or without
`--volume`, reclaims nothing (exit 0).

**Through the node daemon.** The stack's `guepard-node` rebuilt from the exact
source of the running one with only this change added (only the two gfs
Kubernetes crates recompiled). A database created through the console, three
volume generations built in the daemon's repository with the stack's `gfs`
CLI (50 MiB file, 4 commits, `checkout -b`, 50 MiB, commit, `checkout main`;
restore checked), then deleted through the console (`DELETE /api/databases/:id`,
which asks the control plane for `destroy: true`). The daemon logged "deleted
ZFS volumes the driver left behind" for one of the three volumes; 30 s later
none of the three `ZFSVolume` records or datasets was left, and the repository
directory was gone.

**Docker.** Destroy on Docker does not go through this code. On the control
node's Docker, the PR head and the final binary each ran `init`, a row, a
commit, `checkout -b`, a row, a commit, `checkout main` (refused for
uncommitted files both times, then `--force`), and `destroy -y`: the same exit
code at every step, `main` holds row 1 only, and the repository's container
count goes from 1 to 0. `gfs storage status`, `quota` and `clone` on an APFS
directory give the same output with both binaries (`quota`'s used bytes differ
by the 4 KiB the disk changed between the calls). `gfs storage reclaim` with no
kubeconfig fails with "kubernetes client unavailable" (exit 1).

## Not verified

- The race itself is inferred from the driver source and its logs, not
  provoked deterministically; the unfixed binary leaked in 3 of 4 runs here.
- The node daemon was not run with the unfixed code on the flow below, so its
  path is calibrated by the CLI runs and by the fix's own log line, not by a
  daemon leak observed side by side.
- A recorded OpenEBS volume on a cluster without the `ZFSVolume` CRD cannot
  occur (the volume comes from that driver); the 404 handling for it is read
  from the code.
- A clone chain needing more than one pass was not observed in a gfs destroy;
  it was observed with `gfs storage reclaim` (below).
- The 120 s bound was never reached. A volume still held at the deadline is
  logged and left, as before.

## Volumes orphaned before the fix

A destroy finishes the deletes for its own volumes. A volume stranded by a
destroy that ran before the fix — or by anything else that deletes several
snapshots of one volume at once — is reclaimed with:

```sh
gfs storage reclaim                    # lists them; at a terminal, asks before deleting
gfs storage reclaim --yes              # reclaims them without asking
gfs storage reclaim --yes --volume <pv-name> [--volume ...]   # only these
```

It runs where the Kubernetes runtime runs (`KUBECONFIG`), considers only
`ZFSVolume`s the driver marked for deletion, and applies the same rule as
destroy: reclaim when there is no PV and no `ZFSSnapshot` left; report the rest
as `blocked` or `keep` with the reason. Without `--yes`, at a terminal it ends
the list with a y/N question and deletes only on `y`; with `--json`, or when
stdin or stderr is not a terminal (a script, a pipe), it only lists. Once
deleting, it keeps passing over the
set for up to `--wait-secs` (default 120), so a parent whose snapshot a
stranded clone keeps alive goes in the same run, once the clone is gone. The
`used` column comes from `zfs list` and is only filled in on the node that
holds the pool.

Run on this stack:

- The dry run listed the 20 marked volumes: 19 `reclaim`, and 1 `blocked`
  (`the node agent is already destroying it`) — a parent the driver has been
  trying to delete since 2026-10-05, held by the snapshot its stranded clone
  (one of the 19) was cloned from. The volume count was 20 before and after.
- A chain built for the test with `kubectl` alone — volume A with two
  snapshots, volume B cloned from A's second snapshot with two of its own, both
  PVCs deleted, then all four snapshots deleted in one call — reproduced both
  shapes without gfs: B marked with no snapshot and no PV, its 20 MiB still on
  disk; A with a deletion timestamp and one snapshot the driver could not
  destroy. `gfs storage reclaim --volume A --volume B` listed B `reclaim` and A
  `blocked`; with `--yes` it deleted B, and the node agent then finished A in
  the same run (28 s, exit 0). Both records, both datasets and every snapshot
  were gone; no other volume was touched.
- Two volumes stranded by the unfixed gfs destroy (`orphan-b`, `orphan-d`
  above, 89.8 MB each): `gfs storage reclaim --volume <it>` listed it
  `reclaim` and changed nothing (`ZFSVolume` count 26 and 23, before and
  after); `--yes` reclaimed it in 1 s, exit 0. It was the only `ZFSVolume`
  that disappeared, none appeared, and its dataset was gone. The second run
  used the final binary.
- The question, on a third volume the unfixed destroy stranded (89.8 MB), all
  with `--volume <it>`: `y` piped in without `--yes` (not a terminal), only the
  list; at a terminal (`script`), `n` and an empty answer changed nothing, and
  `--json` printed the list without asking; `y` reclaimed it, record and
  dataset gone.
- Before the command existed, deleting the records of the three volumes these
  runs had stranded by hand (`kubectl -n <ns> delete zfsvolume <name>`)
  removed their datasets (240 MB, 89.8 MB, 89.8 MB).
- During the session, two volumes of other databases on the same stack, whose
  destroys did not run through these binaries, ended up marked for deletion
  with no PV (created 18:19 and 19:35). Which client destroyed them was not
  checked; the node daemon's destroy goes through the same
  `remove_instance_with_pvcs`.
