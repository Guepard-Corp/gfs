//! Live schema introspection for a GFS database — calls the existing
//! `ExtractSchemaUseCase` in-process (no subprocess, no SQL reimplemented) and
//! returns the canonical `gfs_domain` `DatasourceMetadata`.

use std::path::Path;
use std::sync::Arc;

use gfs_compute_docker::DockerCompute;
use gfs_domain::model::datasource::DatasourceMetadata;
use gfs_domain::ports::database_provider::InMemoryDatabaseProviderRegistry;
use gfs_domain::usecases::repository::extract_schema_usecase::ExtractSchemaUseCase;
use serde::Serialize;

#[derive(Debug, Serialize)]
pub struct SchemaSnapshot {
    pub reachable: bool,
    pub error: Option<String>,
    pub metadata: Option<DatasourceMetadata>,
}

fn unreachable(error: String) -> SchemaSnapshot {
    SchemaSnapshot { reachable: false, error: Some(error), metadata: None }
}

/// Extract the schema for the repo at `repo` via `ExtractSchemaUseCase`.
pub async fn extract(repo: &str) -> SchemaSnapshot {
    let compute = match DockerCompute::new() {
        Ok(c) => Arc::new(c),
        Err(e) => return unreachable(format!("docker: {e}")),
    };
    let registry = Arc::new(InMemoryDatabaseProviderRegistry::new());
    if let Err(e) = gfs_compute_docker::containers::register_all(registry.as_ref()) {
        return unreachable(format!("provider registry: {e}"));
    }

    match ExtractSchemaUseCase::new(compute, registry).run(Path::new(repo)).await {
        Ok(out) => SchemaSnapshot { reachable: true, error: None, metadata: Some(out.metadata) },
        Err(e) => unreachable(e.to_string()),
    }
}
