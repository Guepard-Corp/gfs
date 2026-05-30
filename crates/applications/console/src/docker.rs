//! Read-only Docker introspection: enumerate the databases GFS launched.
//!
//! GFS stamps every container it provisions with labels (see
//! `init_repo_usecase`): `gfs.managed=true`, `gfs.role=source|clone`,
//! `gfs.provider`, `gfs.provider_version`, `gfs.repo`, and `gfs.remote` (clones).
//! We never create or mutate a container here — only list and inspect.

use bollard::Docker;
use bollard::query_parameters::ListContainersOptionsBuilder;
use serde::Serialize;

const POSTGRES_PORT: u16 = 5432;

/// A GFS-managed database (source or clone) as seen on the local Docker host.
#[derive(Debug, Clone, Serialize)]
pub struct GfsDatabase {
    pub container_id: String,
    pub name: String,
    /// `source` | `clone` (from `gfs.role`).
    pub role: String,
    pub provider: Option<String>,
    pub provider_version: Option<String>,
    pub repo: Option<String>,
    /// `host:port` of the upstream a clone overlays (`gfs.remote`); `None` for sources.
    pub remote: Option<String>,
    pub image: Option<String>,
    /// Docker lifecycle state, e.g. `running`, `exited`.
    pub state: Option<String>,
    pub status: Option<String>,
    /// Host port mapped to the container's 5432/tcp, when published.
    pub host_port: Option<u16>,
    /// Unix seconds the container was created.
    pub created: Option<i64>,
}

/// List every `gfs.managed=true` container (running or stopped).
///
/// Label filtering is done client-side: the container count is tiny and this is
/// robust across Docker API versions (mirrors the proxy's discovery pass).
pub async fn list_databases(docker: &Docker) -> anyhow::Result<Vec<GfsDatabase>> {
    let opts = ListContainersOptionsBuilder::default().all(true).build();
    let containers = docker.list_containers(Some(opts)).await?;

    let mut out = Vec::new();
    for c in &containers {
        let labels = c.labels.as_ref();
        let label = |k: &str| labels.and_then(|l| l.get(k)).cloned();
        if label("gfs.managed").as_deref() != Some("true") {
            continue;
        }

        let Some(container_id) = c.id.clone().filter(|s| !s.is_empty()) else {
            continue;
        };

        let name = c
            .names
            .as_ref()
            .and_then(|n| n.first())
            .map(|n| n.trim_start_matches('/').to_string())
            .unwrap_or_else(|| container_id.clone());

        let host_port = c.ports.as_ref().and_then(|ports| {
            ports
                .iter()
                .find(|p| p.private_port == POSTGRES_PORT && p.public_port.is_some())
                .and_then(|p| p.public_port)
        });

        out.push(GfsDatabase {
            container_id,
            name,
            role: label("gfs.role").unwrap_or_else(|| "source".to_string()),
            provider: label("gfs.provider"),
            provider_version: label("gfs.provider_version"),
            repo: label("gfs.repo"),
            remote: label("gfs.remote"),
            image: c.image.clone(),
            state: c.state.as_ref().map(|s| s.to_string()),
            status: c.status.clone(),
            host_port,
            created: c.created,
        });
    }

    // Stable, newest-first ordering for the UI.
    out.sort_by(|a, b| b.created.cmp(&a.created));
    Ok(out)
}
