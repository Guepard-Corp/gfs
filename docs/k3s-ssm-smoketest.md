## Remote k3s smoke test via AWS SSM

Targets:
- **control-plane**: `guepard-dev-cp`
- **worker**: `guepard-dev-dp`

Assumptions:
- You run these commands from your local machine where AWS SSM is configured.
- You will run `gfs` **on the control-plane** via SSM (no inbound SSH).
- Do **not** commit `.env`.

### 1) Verify instances are reachable via SSM

Use your usual SSM workflow to run:

- On `guepard-dev-cp`: `uname -a`
- On `guepard-dev-dp`: `uname -a`

### 2) Install k3s server on control-plane

On `guepard-dev-cp`:

```bash
curl -sfL https://get.k3s.io | sh -
sudo kubectl get nodes -o wide
sudo cat /var/lib/rancher/k3s/server/node-token
```

### 3) Join worker node

On `guepard-dev-dp` (replace `<CP_PRIVATE_IP>` and `<NODE_TOKEN>`):

```bash
curl -sfL https://get.k3s.io | K3S_URL="https://<CP_PRIVATE_IP>:6443" K3S_TOKEN="<NODE_TOKEN>" sh -
```

Back on `guepard-dev-cp`:

```bash
sudo kubectl get nodes -o wide
```

### 4) Ensure VolumeSnapshot support exists

On `guepard-dev-cp`:

```bash
sudo kubectl get crd | grep -i volumesnapshot || true
sudo kubectl api-resources | grep -i volumesnapshot || true
```

If these are missing, install the snapshot controller + CRDs (cluster-specific; k3s does not always ship them by default).

### 5) Build GFS on the control-plane

On `guepard-dev-cp` (repo location assumed; adjust as needed):

```bash
cd /path/to/gfs
cargo build -p gfs-cli
```

### 6) Initialize a kubernetes-backed repo + Postgres

On `guepard-dev-cp`:

```bash
export KUBECONFIG=/etc/rancher/k3s/k3s.yaml
export GFS_RUNTIME_PROVIDER=kubernetes

mkdir -p /tmp/gfs-k3s-test && cd /tmp/gfs-k3s-test
/path/to/gfs/target/debug/gfs init --database-provider postgres --database-version 17
```

Confirm the repo config has `mount_point = \"<instance>-data\"` (PVC name) and runtime provider is kubernetes:

```bash
cat .gfs/config.toml
sudo kubectl -n gfs get statefulset,svc,pvc
```

### 7) Commit / mutate / checkout

On `guepard-dev-cp`:

```bash
export GFS_ALLOW_UNFROZEN_SNAPSHOT=1

# write data
psql "$(./target/debug/gfs status --output json | jq -r '.compute.connection_string')" -c "create table if not exists t(x int); insert into t values (1);"
./target/debug/gfs commit -m "k3s snapshot 1"

# mutate
psql "$(./target/debug/gfs status --output json | jq -r '.compute.connection_string')" -c "insert into t values (2);"

# checkout back (use hash from gfs log)
HASH="$(./target/debug/gfs log --max-count 1 --full-hash | head -n1 | awk '{print $1}')"
./target/debug/gfs checkout "$HASH"

# verify
psql "$(./target/debug/gfs status --output json | jq -r '.compute.connection_string')" -c "select * from t order by x;"
```

Expected: after checkout, the second insert is gone (table contains only `1`).

