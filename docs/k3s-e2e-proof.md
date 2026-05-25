# GFS k3s E2E proof

**Date:** 2026-05-25  
**Branch:** `feat/k3s-kubernetes-runtime`  
**Commit:** `7a2e36d` (includes CLI fixes + `scripts/k3s-e2e.sh`)  
**Test repo:** `/tmp/gfs-k3s-e2e`  
**Full log:** `/tmp/gfs-k3s-e2e.log` (312 lines)

## Cluster topology

| Role | Host | k3s API | GFS / psql |
|------|------|---------|------------|
| CP | `guepard-dev-cp` `10.0.1.101` / `44.245.9.188` | `KUBECONFIG` → CP (TLS skip for rotated public IP) | — |
| DP (worker) | `guepard-dev-dp` `10.0.1.57` / `52.32.232.60` | — | Postgres NodePort via `GUEPARD_EXTERNAL_HOST` |

Production DP uses `/etc/guepard/environment` + `/etc/guepard/kubeconfig` (CP private IP). This proof run used a laptop-built `gfs` binary with remote `KUBECONFIG` and external psql to worker EIP — same runtime semantics.

## Environment (no secrets)

```bash
KUBECONFIG=/home/guepard/work/gfs/.kube-remote/k3s.yaml   # server https://44.245.9.188:6443, insecure-skip-tls-verify
GFS_RUNTIME_PROVIDER=kubernetes
GFS_ALLOW_UNFROZEN_SNAPSHOT=1
GFS_K8S_STORAGE_CLASS=openebs-zfs-gfs
GFS_K8S_SNAPSHOT_CLASS=openebs-zfs-gfs-snapclass
GFS_K8S_PVC_SIZE_GI=1
GUEPARD_EXTERNAL_HOST=52.32.232.60
```

**Resolved connection (final):** `postgresql://<user>@52.32.232.60:30818/postgres` (password omitted)  
**Final container:** `gfs-pg-1779702350271`

## Preflight

- Applied `deploy/kubernetes/namespace-rbac.yaml`, `openebs-zfs-gfs.yaml`
- Worker SG: TCP 5432 + 30000–32767 from test IP
- CP SG: TCP 6443 from test IP
- **Storage cleanup:** deleted stale `gfs-pg-*` StatefulSets/Services/PVCs (ZFS pool was full → `FailedScheduling: not enough free storage`)

## Pass/fail matrix

| Step | Command | Exit | psql / notes |
|------|---------|------|----------------|
| Init | `gfs init` (postgres 17) | 0 | `PostgreSQL 17.10` via `52.32.232.60:30154` |
| Status | `gfs status` | 0 | Connection uses worker EIP |
| Log | `gfs log` | 0 | empty (new repo) |
| Seed | `create table gfs_e2e; insert init_main` | 0 | 1 row |
| Commit c1 | `gfs commit -m "main c1"` | 0 | VolumeSnapshot created |
| Mutate | insert `after_c1` | 0 | 2 rows |
| Commit c2 | `gfs commit -m "main c2"` | 0 | |
| Checkout c1 | `gfs checkout <hash>` | 0 | **only** `init_main` |
| Checkout main | `gfs checkout main` | 0 | `init_main`, `after_c1` |
| Branch feature | `gfs branch feature` | 0 | |
| Checkout feature | `gfs checkout feature` | 0 | same as main tip |
| Feature commit | insert + `gfs commit -m "feature c1"` | 0 | +`feature_c1` |
| Main c3 | checkout main + insert + commit | 0 | +`main_c3` on main |
| Feature tip | `gfs checkout feature` | 0 | **no** `main_c3` (3 rows, feature branch data) |
| Checkout -b hotfix | `gfs checkout -b hotfix` | 0 | k8s PVC restore OK |
| Branch -c | `gfs branch hotfix2 -c` | 0 | k8s restore OK |
| Branch list | `gfs branch` | 0 | `* hotfix2` |
| Compute stop | `gfs compute stop` | 0 | psql fails (scaled down) |
| Compute start | `gfs compute start` | 0 | psql OK, data retained |
| Compute restart | `gfs compute restart` | 0 | `before_restart` row survives |
| Query | `gfs query "select count(*)..."` | 0 | count = 4 |
| Branch -d | `gfs branch -d hotfix` | 0 | |
| kubectl proof | `kubectl -n gfs get ...` | 0 | see below |

## Key psql snapshots

**After checkout c1 (time travel):**

```
   step    | branch
-----------+--------
 init_main | main
```

**After checkout main (return HEAD):**

```
   step    | branch
-----------+--------
 after_c1  | main
 init_main | main
```

**After feature tip (branch isolation):**

```
    step    | branch
------------+---------
 after_c1   | main
 feature_c1 | feature
 init_main  | main
```

(no `main_c3` on feature branch)

## VolumeSnapshots (this run)

```
gfs-snap-e2c035d46ffc9d8bff185a4fbe75f67a   readyToUse=true   (main c1)
gfs-snap-1c031395c2590716bb0d1bda0636a458   readyToUse=true   (main c2)
gfs-snap-492c049b46f86f922b8259850c98e123   readyToUse=true   (feature c1)
gfs-snap-41fbad9029735c4a86d5dddfdec6fdd5   readyToUse=true   (main c3)
```

## kubectl snapshot (end of run)

```
pod/gfs-pg-1779702350271-0     Running   ip-10-0-1-57
service/...-svc                NodePort  5432:30818/TCP
statefulset/gfs-pg-1779702350271   postgres:17
6x gfs-ws-* PVCs (checkout clones)
4x new VolumeSnapshots (readyToUse=true)
```

## Code fixes included in this branch

| Area | Change |
|------|--------|
| `cmd_branch.rs` | `-c` delegates to `cmd_checkout::checkout` (k8s path) |
| `cmd_query.rs` | `compute_for_repo` instead of hardcoded Docker |
| `cmd_checkout.rs` | k8s `-b` / `create_branch` + `postgres:17` on reprovision |
| `scripts/k3s-e2e.sh` | Full matrix; `--path` + `--json`; `GUEPARD_EXTERNAL_HOST` |

## Warnings (expected)

- `GFS_ALLOW_UNFROZEN_SNAPSHOT=1`: k8s has no cgroup pause; snapshots are best-effort
- Schema DDL missing on commit (warn only)
- `gfs compute stop` status UI may still show “running” briefly; external psql correctly fails

## Re-run

```bash
# Free ZFS if pool full:
kubectl -n gfs delete statefulset,svc --all
kubectl -n gfs delete pvc --all

export KUBECONFIG=... GUEPARD_EXTERNAL_HOST=52.32.232.60
export GFS_DB_USER=... GFS_DB_PASSWORD=...   # required; not stored in repo
./scripts/k3s-e2e.sh
```

On DP (recommended for production parity):

```bash
sudo -i
export $(grep -v '^#' /etc/guepard/environment | xargs)
# install/copy gfs binary, then same script with GFS_BIN=/path/to/gfs
```
