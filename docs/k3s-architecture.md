# GFS + k3s architecture

Git-like version control for databases, backed by Kubernetes storage and compute.

---

## The idea in one sentence

**GFS** tracks database history (commits, branches, checkout). On k3s, each “database workspace” is a **Postgres pod + disk (PVC)**; each **commit** is a **VolumeSnapshot** of that disk; **checkout** restores a new disk from that snapshot and starts a new pod.

---

## Laptop vs data plane

| Where you run `gfs` | k3s access |
|---------------------|------------|
| **Developer laptop** | **Console remote only** — `GFS_RUNTIME_PROVIDER=guepard` or `gfs init --remote`. No `KUBECONFIG` to CP. See [console-remote.md](./console-remote.md). |
| **DP host (SSM)** | `KUBECONFIG=/etc/guepard/kubeconfig` — ops/debug only (`kubectl get pods -n gfs`). |
| **guepard-node** | In-process GFS + k8s adapters; NATS from CP. |

---

## Who talks to whom (DP / legacy direct path)

```mermaid
%%{init: {'theme': 'base', 'themeVariables': { 'primaryColor': '#2ba2bd', 'primaryTextColor': '#f2f2f2', 'primaryBorderColor': '#161616', 'lineColor': '#161616', 'secondaryColor': '#ffcb51', 'tertiaryColor': '#f2f2f2'}}}%%
flowchart LR
  subgraph client["Your machine or DP"]
    CLI["gfs CLI"]
  end
  subgraph cp["Control plane — k3s API"]
    API["k3s API :6443"]
  end
  subgraph cluster["Kubernetes cluster"]
    NS["namespace: gfs"]
    PG["Postgres StatefulSet"]
    PVC["PVC — database files"]
    SNAP["VolumeSnapshot"]
  end
  subgraph worker["Worker node — DP"]
    ZFS["OpenEBS ZFS pool"]
    NP["NodePort / hostPort"]
  end
  CLI -->|"kubectl API"| API
  API --> NS
  NS --> PG
  PG --> PVC
  PVC --> ZFS
  CLI -->|"psql optional"| NP
  NP --> PG
  SNAP -.->|"restore on checkout"| PVC

  classDef gfs fill:#ffcb51,stroke:#161616,color:#161616
  classDef k8s fill:#2ba2bd,stroke:#161616,color:#f2f2f2
  classDef store fill:#f2f2f2,stroke:#161616,color:#161616
  class CLI gfs
  class API,NS,PG k8s
  class PVC,SNAP,ZFS,NP store
```

| Piece | Role |
|-------|------|
| **gfs CLI** | On **DP** or legacy local k8s: talks to k3s API. On **laptop**: use console remote (no direct API). |
| **k3s API (CP)** | Control plane only. Schedules pods, creates PVCs and snapshots. DP kubeconfig points here (`10.0.1.101`), not `127.0.0.1`. |
| **Worker (DP)** | Runs Postgres pods and ZFS-backed volumes. External clients connect via **NodePort** on the worker public IP. |

---

## Glossary (technologies)

| Term | Meaning |
|------|---------|
| **GFS** | *Git For database Systems* — CLI that versions database state like Git versions code. |
| **k3s** | Lightweight Kubernetes distribution (single binary). Your **CP** runs the server; **DP** joins as a worker. |
| **Kubernetes (k8s)** | Orchestrator: deploys containers, networks, storage APIs. GFS uses it when `GFS_RUNTIME_PROVIDER=kubernetes`. |
| **kubectl / kubeconfig** | CLI + credentials to talk to the k3s API. On DP: `KUBECONFIG=/etc/guepard/kubeconfig`. |
| **Namespace `gfs`** | Isolated area in the cluster for all GFS Postgres workloads and PVCs. |
| **StatefulSet** | Kubernetes workload for **one** Postgres pod with a stable name and persistent disk. |
| **PVC** (*PersistentVolumeClaim*) | “Disk request” — Postgres data lives here (`/var/lib/postgresql/data`). |
| **StorageClass** | Template for how PVCs are provisioned. GFS uses **`openebs-zfs-gfs`** (ZFS on the node). |
| **CSI** | *Container Storage Interface* — plugin that creates real volumes on the node. OpenEBS ZFS is a CSI driver. |
| **VolumeSnapshot** | Point-in-time copy of a PVC. GFS creates one per **commit**. |
| **VolumeSnapshotClass** | How snapshots are taken — **`openebs-zfs-gfs-snapclass`**. |
| **NodePort** | Exposes Postgres port `5432` on a high port (30000–32767) on the worker so `psql` can reach it from outside. |
| **OpenEBS ZFS** | Storage engine using a ZFS pool (`zfspv-pool`) on each node — supports snapshots (unlike `local-path`). |
| **Commit** | GFS metadata + snapshot hash; database frozen-ish copy at that moment. |
| **Branch** | Named pointer to a commit (like Git). |
| **Checkout** | Move branch/HEAD to a commit and **restore** DB disk from that commit’s snapshot. |
| **guepard-node** | Optional DP daemon that runs GFS deploys via NATS; raw `gfs` CLI only needs env + kubectl. |

---

## Repo layout vs cluster layout

```mermaid
%%{init: {'theme': 'base'}}%%
flowchart TB
  subgraph local["On disk — GFS repo e.g. /tmp/my-db"]
    CFG[".gfs/config.toml"]
    REFS[".gfs/refs — branches"]
    COMMITS[".gfs/commits — metadata"]
    WS[".gfs/workspaces — layout hints"]
  end
  subgraph k8s["In cluster — namespace gfs"]
    SS["StatefulSet gfs-pg-…"]
    SVC["Service NodePort"]
    PVC2["PVC gfs-pg-…-data"]
    VS["VolumeSnapshot gfs-snap-…"]
  end
  CFG -->|"runtime.container_name"| SS
  CFG -->|"mount_point = PVC name"| PVC2
  COMMITS -->|"snapshot_hash"| VS
  VS -->|"clone on checkout"| PVC2
  SS --> PVC2
  SS --> SVC

  classDef gfs fill:#ffcb51,stroke:#161616,color:#161616
  classDef meta fill:#f2f2f2,stroke:#161616,color:#161616
  classDef k8s fill:#2ba2bd,stroke:#161616,color:#f2f2f2
  class CFG,REFS,COMMITS,WS meta
  class SS,SVC,PVC2,VS k8s
```

- **Local `.gfs/`** = Git-like history (commits, branches, messages). Small files only.
- **Cluster** = actual Postgres + data. Heavy lifting happens in k8s.

---

## Lifecycle: `gfs init`

```mermaid
%%{init: {'theme': 'base'}}%%
sequenceDiagram
  participant U as User / gfs CLI
  participant K as k3s API
  participant S as Storage CSI
  participant P as Postgres pod

  U->>U: Create .gfs repo + config
  U->>K: Create PVC with StorageClass openebs-zfs-gfs
  K->>S: Provision ZFS volume on worker
  U->>K: Create StatefulSet and Service NodePort
  K->>P: Start postgres:17
  P-->>U: Ready on GUEPARD_EXTERNAL_HOST and NodePort
```

1. CLI writes config: `runtime_provider = kubernetes`, `container_name = gfs-pg-<timestamp>`.
2. Kubernetes creates a **1Gi PVC** (configurable) on the worker ZFS pool.
3. Postgres starts; Service publishes **NodePort** for external access.

---

## Lifecycle: `gfs commit`

```mermaid
%%{init: {'theme': 'base'}}%%
sequenceDiagram
  participant U as gfs CLI
  participant P as Postgres
  participant K as k3s API
  participant Z as ZFS / CSI

  U->>P: prepare best-effort, no cgroup pause on k8s
  U->>K: VolumeSnapshot from PVC
  K->>Z: ZFS snapshot
  Z-->>K: readyToUse=true
  U->>U: Write commit file with snapshot_hash
```

- Each commit stores a **snapshot hash** in `.gfs/commits/`.
- Cluster object: `VolumeSnapshot` named `gfs-snap-<hash-prefix>`.
- With `GFS_ALLOW_UNFROZEN_SNAPSHOT=1`, k8s skips Docker-style freeze — fine for dev; not crash-consistent for production unless you add quiesce logic.

---

## Lifecycle: `gfs checkout`

```mermaid
%%{init: {'theme': 'base'}}%%
sequenceDiagram
  participant U as gfs CLI
  participant K as k3s API
  participant Z as ZFS / CSI
  participant P as Postgres

  U->>U: Update HEAD / branch ref
  U->>K: Stop and remove old StatefulSet
  U->>K: Clone PVC from VolumeSnapshot
  K->>Z: New volume from snapshot
  U->>K: New StatefulSet on new PVC postgres:17
  K->>P: Start pod
  P-->>U: DB at commit point in time
```

Checkout is the “time travel” step: new PVC from snapshot → new pod → same rows/schema as at that commit.

---

## Branching

```mermaid
%%{init: {'theme': 'base'}}%%
gitGraph
  commit id: "main c1"
  commit id: "main c2"
  branch feature
  checkout feature
  commit id: "feature c1"
  checkout main
  commit id: "main c3"
```

- **`gfs branch feature`** — creates a ref only (no cluster change).
- **`gfs checkout feature`** — same as commit checkout: restore snapshot for that branch tip.
- **`gfs checkout -b new`** / **`gfs branch new -c`** — create ref + checkout (k8s restore path).

Branches do not share one running pod; each checkout can spawn a **new** StatefulSet + PVC clone.

---

## Adapters in the codebase

```mermaid
%%{init: {'theme': 'base'}}%%
flowchart TB
  CLI["gfs CLI commands"]
  DOM["gfs-domain — use cases"]
  CD["compute-docker"]
  CK["compute-kubernetes"]
  SD["storage-docker / btrfs / …"]
  SK["storage-kubernetes"]
  CLI --> DOM
  DOM --> CD
  DOM --> CK
  DOM --> SD
  DOM --> SK
  CK --> K8S["k3s API"]
  SK --> K8S

  classDef gfs fill:#ffcb51,stroke:#161616,color:#161616
  classDef k8s fill:#2ba2bd,stroke:#161616,color:#f2f2f2
  classDef core fill:#f2f2f2,stroke:#161616,color:#161616
  class CLI,DOM gfs
  class CK,SK,K8S k8s
  class CD,SD core
```

| Adapter | Implements |
|---------|------------|
| **compute-kubernetes** | StatefulSet, Service, exec, start/stop, connection info (NodePort + `GUEPARD_EXTERNAL_HOST`). |
| **storage-kubernetes** | PVC create, VolumeSnapshot create, PVC clone from snapshot. |

`compute_support` picks Docker vs Kubernetes from repo config / `GFS_RUNTIME_PROVIDER`.

---

## Environment variables (runtime)

| Variable | Purpose |
|----------|---------|
| `GFS_RUNTIME_PROVIDER=kubernetes` | Use k8s adapters. |
| `KUBECONFIG` | Path to kubeconfig (CP API, not DP localhost). |
| `GFS_K8S_STORAGE_CLASS` | PVC provisioner (`openebs-zfs-gfs`). |
| `GFS_K8S_SNAPSHOT_CLASS` | Snapshot class name. |
| `GFS_K8S_PVC_SIZE_GI` | PVC size (keep small on dev ZFS pools). |
| `GUEPARD_EXTERNAL_HOST` | Worker IP/hostname in `gfs status` connection string. |
| `GFS_K8S_EXPOSE_NODEPORT` | Expose Postgres via NodePort (default on). |
| `GFS_ALLOW_UNFROZEN_SNAPSHOT` | Allow commit without cgroup pause on k8s. |

---

## Production-style deployment (CP + DP)

```mermaid
%%{init: {'theme': 'base'}}%%
flowchart TB
  subgraph cp["guepard-dev-cp"]
    K3S["k3s server"]
    CPAPI["guepard-control-plane :8000"]
    NATS["NATS"]
  end
  subgraph dp["guepard-dev-dp"]
    NODE["guepard-node"]
    GFS2["gfs CLI"]
    ENV["/etc/guepard/environment"]
  end
  subgraph ext["External"]
    PSQL["psql / app"]
  end
  GFS2 --> ENV
  GFS2 -->|"KUBECONFIG → CP"| K3S
  NODE --> GFS2
  NODE --> NATS
  CPAPI --> NATS
  PSQL -->|"NodePort"| dp
  K3S --> dp

  classDef gfs fill:#ffcb51,stroke:#161616,color:#161616
  classDef k8s fill:#2ba2bd,stroke:#161616,color:#f2f2f2
  classDef infra fill:#f2f2f2,stroke:#161616,color:#161616
  class GFS2,NODE,PSQL gfs
  class K3S k8s
  class CPAPI,NATS,ENV infra
```

- Run **`gfs`** on **DP** with CP kubeconfig.
- Connect clients to **DP public IP** + NodePort (or fixed `GFS_K8S_POSTGRES_NODE_PORT` if in range).
- **`guepard-node`** is optional for console-driven deploys; not required for manual CLI testing.

---

## What GFS does *not* do on k8s

- No in-cluster Git mirror of SQL — metadata stays in `.gfs/` on the host.
- No cgroup pause on k8s (unlike Docker); snapshots are best-effort unless extended.
- Does not manage OpenEBS install — you apply `deploy/kubernetes/openebs-zfs-gfs.yaml` and ZFS pools separately.

---

## Related docs

- [deploy/kubernetes/README.md](../deploy/kubernetes/README.md) — apply manifests, env vars.
- [k3s-ssm-smoketest.md](./k3s-ssm-smoketest.md) — cluster bootstrap via SSM.
- [k3s-e2e-proof.md](./k3s-e2e-proof.md) — full CLI matrix test results.
