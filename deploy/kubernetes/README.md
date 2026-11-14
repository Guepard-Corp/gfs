## k3s (remote) notes

GFS can run its database workspace on a k3s cluster when `runtime.runtime_provider = kubernetes`.

### Prereqs

- **k3s (or any Kubernetes) is installed** (a control-plane node and at least one worker node)
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


### TLS certificate for the database

gfs-postgres images serve TLS with a per-container self-signed certificate. To serve your own
(for `sslmode=verify-full`), create a `kubernetes.io/tls` Secret in the namespace and name it:

```bash
kubectl -n gfs create secret tls gfs-db-tls --cert=tls.crt --key=tls.key
export GFS_K8S_TLS_SECRET=gfs-db-tls
```

Every Postgres pod gfs creates afterwards (init, checkout) mounts it read-only at
`/etc/gfs-tls` (`tls.crt` 0444, `tls.key` 0400, read by the root entrypoint, which copies them
for the postgres user). A missing Secret keeps the pod in `ContainerCreating` rather than
falling back to a self-signed certificate. Unset, nothing is mounted.
