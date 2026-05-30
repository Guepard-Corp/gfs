//! Docker auto-discovery of GFS clone containers.
//!
//! Lists containers labelled `gfs.managed=true` / `gfs.role=clone` /
//! `gfs.provider=postgres` (set by `gfs clone`, see the init use case), fronts
//! each on its own listener (`--listen-base` + next free port), and reconciles on
//! a timer so clones created/removed at runtime are tracked. The clone→port
//! mapping is published at `GET /clones`, served from the same HTTP server as
//! `/metrics`.
//!
//! The proxy still only ever *connects* to the databases; Docker is read-only and
//! used purely to find them — no container is ever created or mutated here.

use std::collections::{BTreeMap, BTreeSet};
use std::convert::Infallible;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bollard::Docker;
use bollard::query_parameters::ListContainersOptionsBuilder;
use http_body_util::Full;
use hyper::body::{Bytes, Incoming};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

use crate::config::Config;

const POSTGRES_PORT: u16 = 5432;

/// One discovered clone we are fronting.
struct Clone {
    name: String,
    backend: String,
    listen_port: u16,
    remote: Option<String>,
    /// Aborted when the clone disappears, which drops its listener (frees port).
    task: JoinHandle<()>,
}

#[derive(Default)]
struct Registry {
    /// container id → fronted clone.
    clones: BTreeMap<String, Clone>,
    /// listen ports currently in use (so a freed port can be reassigned).
    used_ports: BTreeSet<u16>,
}

impl Registry {
    /// Lowest free port at or above `base`.
    fn next_port(&self, base: u16) -> Option<u16> {
        (base..=u16::MAX).find(|p| !self.used_ports.contains(p))
    }
}

type Shared = Arc<Mutex<Registry>>;

pub async fn run(cfg: Config) -> anyhow::Result<()> {
    let docker = Docker::connect_with_local_defaults()
        .map_err(|e| anyhow::anyhow!("--discover needs Docker, but connecting failed: {e}"))?;
    docker
        .ping()
        .await
        .map_err(|e| anyhow::anyhow!("--discover needs Docker, but ping failed: {e}"))?;

    let registry: Shared = Arc::new(Mutex::new(Registry::default()));

    // Combined /metrics + /clones server (the Prometheus recorder is installed
    // without its own listener so we own the single HTTP endpoint).
    let handle = crate::telemetry::install_recorder()?;
    tokio::spawn(serve_http(cfg.metrics, handle, registry.clone()));

    tracing::info!(
        listen_base = cfg.listen_base,
        interval_s = cfg.discover_interval,
        metrics = %cfg.metrics,
        "discovery mode: watching Docker for GFS clones (GET /clones for the live map)"
    );

    let interval = Duration::from_secs(cfg.discover_interval.max(1));
    loop {
        if let Err(e) = reconcile(&docker, &cfg, &registry).await {
            tracing::warn!(error = %e, "discovery reconcile failed; retrying");
        }
        tokio::time::sleep(interval).await;
    }
}

/// One pass: start listeners for newly-seen clones, stop them for vanished ones.
async fn reconcile(docker: &Docker, cfg: &Config, registry: &Shared) -> anyhow::Result<()> {
    // List running containers and match labels client-side. (Server-side label
    // filtering via the Docker API is finicky to serialize across versions; the
    // container count here is tiny, so a local filter is both simpler and robust.)
    let opts = ListContainersOptionsBuilder::default().all(false).build();
    let containers = docker.list_containers(Some(opts)).await?;

    let mut seen = BTreeSet::new();
    for c in &containers {
        let labels = c.labels.as_ref();
        let label = |k: &str| labels.and_then(|l| l.get(k)).map(String::as_str);
        if label("gfs.managed") != Some("true")
            || label("gfs.role") != Some("clone")
            || label("gfs.provider") != Some("postgres")
        {
            continue;
        }

        let Some(id) = c.id.clone().filter(|s| !s.is_empty()) else {
            continue;
        };
        seen.insert(id.clone());

        // Already fronting it.
        if registry.lock().unwrap().clones.contains_key(&id) {
            continue;
        }

        // Need the host-published 5432/tcp port; a clone mid-creation may not have
        // it yet — skip and pick it up on the next pass.
        let Some(host_port) = c.ports.as_ref().and_then(|ports| {
            ports
                .iter()
                .find(|p| p.private_port == POSTGRES_PORT && p.public_port.is_some())
                .and_then(|p| p.public_port)
        }) else {
            tracing::debug!(container = %id, "clone has no published 5432/tcp yet; will retry");
            continue;
        };

        let name = c
            .names
            .as_ref()
            .and_then(|n| n.first())
            .map(|n| n.trim_start_matches('/').to_string())
            .unwrap_or_else(|| id.clone());
        let remote = c
            .labels
            .as_ref()
            .and_then(|l| l.get("gfs.remote").cloned());

        start_clone(cfg, registry, id, name, host_port, remote);
    }

    // Stop listeners for clones that are gone.
    let mut reg = registry.lock().unwrap();
    let gone: Vec<String> = reg
        .clones
        .keys()
        .filter(|id| !seen.contains(*id))
        .cloned()
        .collect();
    for id in gone {
        if let Some(clone) = reg.clones.remove(&id) {
            clone.task.abort();
            reg.used_ports.remove(&clone.listen_port);
            tracing::info!(
                container = %clone.name,
                listen_port = clone.listen_port,
                "clone gone — stopped its listener"
            );
        }
    }
    Ok(())
}

/// Assign a listen port and spawn a `serve_backend` task for one clone.
fn start_clone(
    cfg: &Config,
    registry: &Shared,
    id: String,
    name: String,
    host_port: u16,
    remote: Option<String>,
) {
    let backend = format!("127.0.0.1:{host_port}");

    // Reserve the port (no await held across the lock).
    let listen_port = {
        let mut reg = registry.lock().unwrap();
        match reg.next_port(cfg.listen_base) {
            Some(p) => {
                reg.used_ports.insert(p);
                p
            }
            None => {
                tracing::error!("no free listen port at/above {}", cfg.listen_base);
                return;
            }
        }
    };

    let mut clone_cfg = cfg.clone();
    clone_cfg.backend = Some(backend.clone());
    clone_cfg.listen = SocketAddr::from((Ipv4Addr::LOCALHOST, listen_port));
    clone_cfg.discover = false; // a per-clone server is a plain single backend

    let backend_for_task = backend.clone();
    let task = tokio::spawn(async move {
        if let Err(e) = crate::proxy::serve_backend(clone_cfg, backend_for_task).await {
            tracing::warn!(error = %e, "clone listener exited");
        }
    });

    tracing::info!(
        container = %name,
        %backend,
        listen_port,
        remote = remote.as_deref().unwrap_or("?"),
        "fronting discovered clone (connect at 127.0.0.1:{listen_port})"
    );

    registry.lock().unwrap().clones.insert(
        id,
        Clone {
            name,
            backend,
            listen_port,
            remote,
            task,
        },
    );
}

// ---------------------------------------------------------------------------
// HTTP: /metrics (Prometheus) + /clones (discovery map)
// ---------------------------------------------------------------------------

async fn serve_http(
    addr: SocketAddr,
    handle: metrics_exporter_prometheus::PrometheusHandle,
    registry: Shared,
) {
    let listener = match TcpListener::bind(addr).await {
        Ok(l) => l,
        Err(e) => {
            tracing::error!(%addr, error = %e, "could not bind metrics/clones HTTP server");
            return;
        }
    };
    tracing::info!(%addr, "serving /metrics and /clones");
    loop {
        let (stream, _) = match listener.accept().await {
            Ok(v) => v,
            Err(e) => {
                tracing::debug!(error = %e, "http accept failed");
                continue;
            }
        };
        let handle = handle.clone();
        let registry = registry.clone();
        tokio::spawn(async move {
            let io = TokioIo::new(stream);
            let svc = service_fn(move |req: Request<Incoming>| {
                let handle = handle.clone();
                let registry = registry.clone();
                async move { Ok::<_, Infallible>(route(req, &handle, &registry)) }
            });
            if let Err(e) = http1::Builder::new().serve_connection(io, svc).await {
                tracing::trace!(error = %e, "http connection error");
            }
        });
    }
}

fn route(
    req: Request<Incoming>,
    handle: &metrics_exporter_prometheus::PrometheusHandle,
    registry: &Shared,
) -> Response<Full<Bytes>> {
    match req.uri().path() {
        "/metrics" => text(handle.render()),
        "/clones" => json(clones_json(registry)),
        _ => not_found(),
    }
}

/// `{ "clones": [ { container, backend, listen_port, remote } ] }`, hand-built
/// via serde_json so the endpoint is stable regardless of internal types.
fn clones_json(registry: &Shared) -> String {
    let reg = registry.lock().unwrap();
    let arr: Vec<serde_json::Value> = reg
        .clones
        .values()
        .map(|c| {
            serde_json::json!({
                "container": c.name,
                "backend": c.backend,
                "listen_port": c.listen_port,
                "remote": c.remote,
            })
        })
        .collect();
    serde_json::json!({ "clones": arr }).to_string()
}

fn text(body: String) -> Response<Full<Bytes>> {
    Response::builder()
        .header("content-type", "text/plain; version=0.0.4")
        .body(Full::new(Bytes::from(body)))
        .unwrap()
}

fn json(body: String) -> Response<Full<Bytes>> {
    Response::builder()
        .header("content-type", "application/json")
        .body(Full::new(Bytes::from(body)))
        .unwrap()
}

fn not_found() -> Response<Full<Bytes>> {
    Response::builder()
        .status(StatusCode::NOT_FOUND)
        .body(Full::new(Bytes::from_static(b"not found\n")))
        .unwrap()
}
