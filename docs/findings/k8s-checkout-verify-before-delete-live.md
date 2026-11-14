# Does the reordered Kubernetes checkout refuse without destroying anything?

**Question.** `9033b92` moves the readiness check on the target commit's
VolumeSnapshot ahead of every destructive step. In the adapter the check now
runs before `teardown_instance_keep_snapshots` and `delete_pvc`. In the CLI it
runs before `create_branch`, `compute.stop` and `repository.checkout`, which
writes `.gfs/HEAD` and `.gfs/WORKSPACE`. On a live cluster, does a checkout
to a commit whose snapshot is missing or unready now leave the StatefulSet,
the data PVC, HEAD and the data exactly as they were? Does the unfixed binary,
given the same input, really destroy them?

**Answer: yes on both counts.** With the fixed binary, a missing snapshot
(A), a missing snapshot under `checkout -b` (C) and an unready snapshot (D)
are each refused with exit 1 and an error naming the snapshot. In every case
the StatefulSet keeps its UID, the PVC keeps its PV, HEAD, WORKSPACE and refs
are unchanged, and both rows are still readable. The unfixed binary, given the
same A and C inputs, deletes the StatefulSet and the PVC and moves HEAD (A) or
creates the branch and moves HEAD onto it (C). The normal checkout still works
(B).

Measured on 2026-10-07, 11:46–11:53Z, on the local two-node k3s stack. Everything
below was run by me unless marked *relayed*.

## Setup

| | |
| --- | --- |
| Cluster | multipass `guepard-dev-cp` (k3s v1.34.6+k3s1 server, <control-plane VM address>), `guepard-dev-dp` (worker, ZFS pool, <worker VM address>) |
| Storage | StorageClass `openebs-zfs-gfs` (zfs.csi.openebs.io, reclaim Delete), VolumeSnapshotClass `openebs-zfs-gfs-snapclass` (deletion policy Delete) |
| Fixed binary | `git archive 9033b92`, sha256 `fdc330df122fe2a0…` |
| Unfixed binary | `git archive ee4197e` (`origin/main`), sha256 `ececf700f6d420b8…` |
| Compiler | both built with `cargo build -p gfs-cli` in a `rust:1.93.1-bookworm` container (rustc 1.93.1 `01f6ddf75`, embedded in both binaries), aarch64 Linux |
| Where gfs ran | inside `guepard-dev-cp`, `KUBECONFIG` = a copy of `/etc/rancher/k3s/k3s.yaml` |
| Environment | as `scripts/k3s-e2e.sh`: `GFS_RUNTIME_PROVIDER=kubernetes`, `GFS_ALLOW_UNFROZEN_SNAPSHOT=1`, `GFS_K8S_STORAGE_CLASS`, `GFS_K8S_SNAPSHOT_CLASS`, `GFS_K8S_PVC_SIZE_GI=1`, `GFS_K8S_EXTERNAL_HOST=<worker VM address>` |

The refusal string the fix adds ("Nothing was changed: HEAD was not moved")
occurs once in the fixed binary and zero times in the unfixed one. That is a
cheap check that the two binaries are not the same build.

*Relayed:* the branch's unit, clippy and fmt gates were re-run separately by
the lead under rustup 1.93.1: 60 + 1 tests pass, and
`clippy -D warnings` is clean. I did not re-run them.

### Why Linux binaries built in a container, not the Mac build

The first attempt used binaries built on macOS (`cargo build` in each
worktree). Two things ruled them out:

- **`gfs commit` on macOS never produces a VolumeSnapshot.** The
  `storage_for_repo` that returns `KubernetesStorage` is
  `#[cfg(target_os = "linux")]` (`cmd_commit.rs`). On macOS the commit
  takes the APFS `cp -cRp` path and failed with
  `cp: gfs-pg-…-data: No such file or directory`. The checkout arm is not
  gated, so on a Mac a Kubernetes repository can be checked out but never
  committed.
- **The Mac builds used the wrong compiler.** `cargo -vV` reported 1.93.1,
  but rustc resolved to Homebrew 1.96.1 (`31fca3adb`, embedded in both Mac
  binaries) because the cargo shim's PATH puts `/opt/homebrew/bin` first.
  The container build uses the pinned 1.93.1.

### How data was read

`gfs status --json` reports `.compute.connection_string` with the in-cluster
Service DNS name, which is unreachable from outside the cluster. The
NodePort is reachable over TCP, but the pod's `pg_hba.conf` is sealed to
loopback by design (`compute-kubernetes/src/lib.rs`, the `gfs-seal-hba` init
container). External auth is opened only by the data-platform node daemon
after reconcile, and the bare CLI never opens it. So every row count below
comes from
`kubectl -n gfs exec <instance>-0 -c db -- psql -U gfs -d postgres -Atc …`
over loopback, which does not depend on the binary under test. A side effect:
`psql_cs` in `scripts/k3s-e2e.sh` cannot authenticate from the Mac either.

### Common preparation (every scenario)

Each scenario used a fresh repository in `/tmp/gfs-k8s-verify/<scenario>`
inside the VM:

```text
+ gfs init --database-provider postgres --database-version 17 --database-user gfs --database-password … --database-name postgres <dir>
  (wait for postgres over loopback exec)
  create table t(id int primary key, v text); insert into t values (1,'row1')
+ gfs commit -m c1            exit=0
  insert into t values (2,'row2')
+ gfs commit -m c2            exit=0
```

C1 and C2 were read from `.gfs/refs/heads/main` after each commit, and each
commit's `snapshot_hash` from its object. The VolumeSnapshot name is
`gfs-snap-<first 32 hex of snapshot_hash>`, the same rule as
`volumesnapshot_name_for_hash`. For example, scenario A:

```text
C1=91fb4685…c34 snapshot_hash=e50c6953…e8f vs=gfs-snap-e50c695382c802bea47a8d91a6017368
C2=1f833783…648 snapshot_hash=082d8382…7b4 vs=gfs-snap-082d8382467832b14643db7ac148c312
```

Both snapshots report `READY=false` for one to two seconds after the commit
returns, then `true`. This matches the documented `snapshotHandle` fast path
in `wait_snapshot_ready`. Before a snapshot was deleted, the script checked
that `.spec.source.persistentVolumeClaimName` was this repository's own
`<instance>-data`, and refused otherwise.

## A. Target snapshot missing (fixed binary)

```text
+ kubectl -n gfs delete volumesnapshot gfs-snap-e50c695382c802bea47a8d91a6017368 --wait=true --timeout=120s   exit=0
+ gfs checkout 91fb468526a620751384efff11899b87117ea92421f83e82b596bc1293799c34
exit=1
error: cannot checkout '91fb4685…c34': the VolumeSnapshot 'gfs-snap-e50c695382c802bea47a8d91a6017368'
recorded by commit 91fb468 is not restorable (internal error: get volumesnapshot failed: ApiError:
volumesnapshots.snapshot.storage.k8s.io "gfs-snap-e50c695382c802bea47a8d91a6017368" not found: NotFound …).
Nothing was changed: HEAD was not moved and the database is still running on its current volume
```

| instance `gfs-pg-1791373569659` | before checkout (11:46:21) | after (11:46:22) | +20 s (11:46:43) |
| --- | --- | --- | --- |
| StatefulSet | READY 1, UID `d2c22e1a…` | READY 1, UID `d2c22e1a…` | READY 1, UID `d2c22e1a…` |
| PVC `-data` | Bound, PV `pvc-897c5bc4…` | Bound, PV `pvc-897c5bc4…` | Bound, PV `pvc-897c5bc4…` |
| `.gfs/HEAD` | `ref: refs/heads/main` | `ref: refs/heads/main` | `ref: refs/heads/main` |
| `.gfs/WORKSPACE` | `…/workspaces/main/0/data` | unchanged | unchanged |
| `refs/heads` | `main = 1f833783…` | `main = 1f833783…` | `main = 1f833783…` |
| rows | 1, 2 | 1, 2 | 1, 2 |

**Verdict: as claimed.** The refusal changed nothing, the same StatefulSet
object (same UID) is still serving, and the same PV is still bound.

## A, calibration. Same input, unfixed binary (`ee4197e`)

```text
+ kubectl -n gfs delete volumesnapshot gfs-snap-fb31b11ff26aafffcc84e1d2f33537a9 …   exit=0
+ gfs checkout 711060a1f0e26a7740593dbd3bd80cf580619f7ee07432ec6788b30f61b570a0
exit=1
error: storage: internal error: get volumesnapshot failed: ApiError: volumesnapshots.snapshot.storage.k8s.io
"gfs-snap-fb31b11ff26aafffcc84e1d2f33537a9" not found: NotFound …
```

| instance `gfs-pg-1791373615423` | before checkout (11:47:06) | after (11:47:08) and +20 s |
| --- | --- | --- |
| StatefulSet | READY 1, UID `27e4eb4e…` | **NotFound** (SuccessfulDelete event 11:47:07Z) |
| PVC `-data` | Bound, PV `pvc-ce52f015…` | **gone**, and PV `pvc-ce52f015…` NotFound |
| `.gfs/HEAD` | `ref: refs/heads/main` | **`711060a1…`** (detached at the refused commit) |
| `.gfs/WORKSPACE` | `…/workspaces/main/0/data` | **`…/workspaces/detached/711060a1f0e2/data`** |
| `refs/heads` | `main = 3db9183a…` | `main = 3db9183a…` |
| rows | 1, 2 | **pod NotFound**, no database |

**Verdict: the check discriminates.** The same input that the fixed binary
refused harmlessly leaves the unfixed repository with no StatefulSet, no PVC,
no PV, and HEAD pointing at a commit it cannot restore. The error does not say
that anything was destroyed.

**Unexpected, observed but not investigated.** At 11:47:40Z, after the PV
object was gone, the ZFS dataset `zfspv-pool/pvc-ce52f015…` and its
`zfsvolume` CR (state Ready, no deletionTimestamp) still existed on the DP
node. They held one ZFS snapshot, the one backing c2's VolumeSnapshot. After
`gfs destroy` removed that VolumeSnapshot, both were gone. So with a sibling
snapshot present, the bytes outlived the PV for a while. The tracker's phrase
"the ZFS volume is reclaimed, not released" is imprecise for that case,
though nothing in Kubernetes still pointed at the data. No uncommitted writes
existed in this run, so this does not measure how much would have been lost.

## B. Happy path, both snapshots present (fixed binary)

| | before | `gfs checkout <c1>` | `gfs checkout main` |
| --- | --- | --- | --- |
| exit | | 0 (`✓ Switched to 8fec4e49… (8fec4e4)`) | 0 (`✓ Switched to main (4fcbad8)`) |
| StatefulSet | READY 1, UID `3dbd6c1c…` | READY 1, UID `97148c4f…` | READY 1, UID `aa43ba06…` |
| PVC `-data` | Bound, PV `pvc-537bd2a6…` | Bound, PV `pvc-8784ba01…` | Bound, PV `pvc-43771b8a…` |
| `.gfs/HEAD` | `ref: refs/heads/main` | `8fec4e493b02…46f1` (bare c1 hash) | `ref: refs/heads/main` |
| rows | 1, 2 | **1** | **1, 2** |

At the moment of the first checkout, c2's snapshot still reported
`READY=false`, and c1's was `true`. Postgres answered 7 s after the first
checkout returned and 8 s after the second. **Verdict: the reorder did not
break a normal checkout.** It restores c1's state into a new PV, and
checking out main restores c2's.

## C. `checkout -b` with a missing snapshot (fixed binary)

```text
+ kubectl -n gfs delete volumesnapshot gfs-snap-0bc07a3989dd63a7891ab5cd1fdd8572 …   exit=0
+ gfs checkout -b feat fd8e36a2a7aa170a4e1cdacd4e60a6dbdf9915c7bbe1417dd03adf8b15f1d747
exit=1
error: cannot checkout 'feat': the VolumeSnapshot 'gfs-snap-0bc07a3989dd63a7891ab5cd1fdd8572' recorded by
commit fd8e36a is not restorable (… NotFound …). Nothing was changed: HEAD was not moved and the database
is still running on its current volume
```

Before and after: StatefulSet READY 1, UID `fd4b7701…`. PVC Bound, PV
`pvc-ce4ab658…`. HEAD `ref: refs/heads/main`. `ls .gfs/refs/heads` →
`main` only. Rows 1, 2. **Verdict: as claimed. No `feat` ref was created.**

*Calibration (unfixed binary, same input, instance `gfs-pg-1791373746072`):*
exit 1 with a bare NotFound error. Afterwards `ls .gfs/refs/heads` → **`feat main`**
(`feat = 0e7757e1…`, the refused commit), HEAD **`ref: refs/heads/feat`**,
WORKSPACE `…/workspaces/feat/0/data`, StatefulSet **NotFound**, PVC (PV
`pvc-f94b378f…` before) **gone**, pod NotFound.

## D. Snapshot present but never ready (fixed binary)

c1's real snapshot was deleted. A VolumeSnapshot with the same name was
then applied with source PVC `<instance>-does-not-exist`. The snapshot
controller reported
`{"error":{"message":"Failed to create snapshot content … cannot get claim from snapshot"},"readyToUse":false}`.

```text
+ [11:49:21] gfs checkout 16f80c6a0761cf60108d60087369ae4be3b4cb354d708ffddbfad3307b897ef4
exit=1 [11:52:42]
error: cannot checkout '16f80c6a…7ef4': the VolumeSnapshot 'gfs-snap-543695f7bc50f836d4bcec129a6e3279'
recorded by commit 16f80c6 is not restorable (internal error: volumesnapshot
'gfs-snap-543695f7bc50f836d4bcec129a6e3279' was not captured in time). Nothing was changed: HEAD was not
moved and the database is still running on its current volume
```

Before and after: StatefulSet READY 1, UID `b7fcf514…`. PVC Bound, PV
`pvc-e4657dd7…`. HEAD `ref: refs/heads/main`. `main = 014289e5…`. Rows 1, 2.
**Verdict: as claimed.** The refusal took 3 min 21 s, which is the full
1800 × 100 ms poll in `wait_snapshot_ready` plus API latency. The database
kept serving throughout, because nothing is stopped before the check.
No calibration was run for D.

## Not covered

- The deletion-in-progress branch of `wait_snapshot_ready` (a snapshot with
  `deletionTimestamp`) was not exercised. Every deleted snapshot here
  finished deleting within about a second.
- The adapter's own check (inside `restore_database_volume_from_snapshot`)
  was never reached alone on the cluster, because the CLI's earlier check
  refuses first in every scenario. Its ordering is covered only by the unit
  tests with the recording fake.
- The `pvc-as-source-protection` wedge (tracked separately) did not occur. In the
  calibration the finalizer was already gone once the snapshots had been
  captured, and the PVC drained immediately.
- No uncommitted writes were present in any scenario, so nothing here
  measures what the unfixed order loses beyond the live volume itself.

## Interference, and how it was excluded

Two earlier attempts the same morning were aborted. A concurrent Claude Code
session working in `data-platform-v3` deleted this run's StatefulSet and PVC
by name, at 11:05:26Z and 11:13:07Z (from its transcript), treating them as
its own orphans. For the measured run, a watcher polled
`kubectl -n gfs get sts,svc,pvc,volumesnapshot,pod` every 3 s. It logged
78 changes between 11:38 and 11:53Z, and every one belongs to an instance
this run created. The watcher was shown to fire: with an empty registry it
flagged this run's own objects. It also scanned other sessions' transcripts
for a `delete <kind> <my-instance>` command. That check flagged the two known
deletions and passed over a session that had only listed the namespace.
Nothing fired during the measured run.

## Cleanup

`gfs destroy -y` exited 0 for all six repositories (A, A-calibration, B, C,
C-calibration, D). D's hand-made VolumeSnapshot does not belong to any commit,
so `destroy` did not reclaim it, and it was deleted by name. Afterwards:
namespace `gfs` has no objects, there are no VolumeSnapshotContents, none of
the eight PVs this run created exists as a PV or as a `zfsvolume`, and none of
the 5 datasets on the DP pool belongs to this run.
