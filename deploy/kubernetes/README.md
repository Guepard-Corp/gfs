## k3s (remote) notes

GFS can run its database workspace on a k3s cluster when `runtime.runtime_provider = kubernetes`.

### Prereqs

- **k3s is installed** (control-plane on `guepard-dev-cp`, worker on `guepard-dev-dp`)
- **VolumeSnapshot CRDs + controller are installed**
  - If they are missing, `gfs commit`/`gfs checkout` will fail when trying to create/restore snapshots.

### Install namespace + RBAC

Apply:

- `deploy/kubernetes/namespace-rbac.yaml`

### Runtime selection

At init time, select k8s runtime via env:

- `GFS_RUNTIME_PROVIDER=kubernetes`

GFS uses kubeconfig discovery (on the control plane this is typically):

- `KUBECONFIG=/etc/rancher/k3s/k3s.yaml`

### OpenEBS ZFS (CSI snapshots)

`local-path` is not CSI-backed and cannot be snapshotted. Use OpenEBS ZFS:

```bash
# On each k3s node: create pool zfspv-pool (see docs/k3s-ssm-smoketest.md)
kubectl apply -f deploy/kubernetes/openebs-zfs-gfs.yaml

export GFS_K8S_STORAGE_CLASS=openebs-zfs-gfs
export GFS_K8S_SNAPSHOT_CLASS=openebs-zfs-gfs-snapclass
export GFS_K8S_PVC_SIZE_GI=5
```

Local laptop against remote API (EIP):

```bash
export KUBECONFIG=/path/to/k3s-remote.yaml   # server: https://<cp-eip>:6443
export GFS_RUNTIME_PROVIDER=kubernetes
export GFS_ALLOW_UNFROZEN_SNAPSHOT=1
export GFS_K8S_STORAGE_CLASS=openebs-zfs-gfs
```

