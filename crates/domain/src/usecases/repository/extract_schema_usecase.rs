//! Use case for extracting schema metadata from a running database instance.
//!
//! Orchestration:
//! 1. Load repo config to get the provider name and container name.
//! 2. Resolve the provider from the registry.
//! 3. Get schema extraction spec from the provider (runs in a container, no host tools required).
//! 4. Run the schema extraction task linked to the database container.
//! 5. Parse stdout output into DatasourceMetadata.
//! 6. Return SchemaOutput with populated metadata.

use std::path::Path;
use std::sync::Arc;

use crate::model::config::GfsConfig;
use crate::model::datasource::{Column, DatasourceMetadata, Schema, Table};
use crate::ports::compute::{Compute, ComputeError, InstanceId};
use crate::ports::database_provider::{ConnectionParams, DatabaseProviderRegistry};

// ---------------------------------------------------------------------------
// Error
// ---------------------------------------------------------------------------

#[derive(Debug, thiserror::Error)]
pub enum ExtractSchemaError {
    #[error("repository not configured for compute: {0}")]
    NotConfigured(String),

    #[error("database provider not found: '{0}'")]
    ProviderNotFound(String),

    #[error("compute error: {0}")]
    Compute(#[from] ComputeError),

    #[error("schema extraction failed: {0}")]
    ExtractionFailed(String),

    #[error("config error: {0}")]
    Config(String),

    #[error("query execution failed: {0}")]
    QueryFailed(String),

    #[error("json parsing failed: {0}")]
    JsonParsing(String),
}

// ---------------------------------------------------------------------------
// Output
// ---------------------------------------------------------------------------

/// Result of a successful schema extraction operation.
#[derive(Debug)]
pub struct SchemaOutput {
    /// Complete schema metadata including schemas, tables, columns, and relationships.
    pub metadata: DatasourceMetadata,
}

// ---------------------------------------------------------------------------
// Delimiters for parsing schema extraction output (from sidecar stdout)
// ---------------------------------------------------------------------------

const DELIM_VERSION: &str = "GFS_SCHEMA_VERSION";
const DELIM_SCHEMAS: &str = "GFS_SCHEMA_SCHEMAS";
const DELIM_TABLES: &str = "GFS_SCHEMA_TABLES";
const DELIM_COLUMNS: &str = "GFS_SCHEMA_COLUMNS";

// ---------------------------------------------------------------------------
// Use case
// ---------------------------------------------------------------------------

pub struct ExtractSchemaUseCase<R: DatabaseProviderRegistry> {
    compute: Arc<dyn Compute>,
    registry: Arc<R>,
}

impl<R: DatabaseProviderRegistry> ExtractSchemaUseCase<R> {
    pub fn new(compute: Arc<dyn Compute>, registry: Arc<R>) -> Self {
        Self { compute, registry }
    }

    /// Extract schema metadata from the database instance associated with the repo at `path`.
    ///
    /// Runs schema extraction inside a container (no psql or other client tools required on host).
    ///
    /// - `path`: GFS repository root.
    pub async fn run(&self, path: &Path) -> Result<SchemaOutput, ExtractSchemaError> {
        // 1. Load repo config.
        let config =
            GfsConfig::load(path).map_err(|e| ExtractSchemaError::Config(e.to_string()))?;

        let provider_name = config
            .environment
            .as_ref()
            .map(|e| e.database_provider.as_str())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                ExtractSchemaError::NotConfigured(
                    "no database provider configured (run gfs init --database-provider <name>)"
                        .into(),
                )
            })?
            .to_string();

        let container_name = config
            .runtime
            .as_ref()
            .map(|r| r.container_name.as_str())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                ExtractSchemaError::NotConfigured(
                    "no container configured (run gfs compute start)".into(),
                )
            })?
            .to_string();

        // 2. Resolve provider.
        let provider = self
            .registry
            .get(&provider_name)
            .ok_or_else(|| ExtractSchemaError::ProviderNotFound(provider_name.clone()))?;

        let instance_id = InstanceId(container_name);

        // 3. Get connection info for the sidecar (container IP, reachable from linked task).
        let conn_info = self
            .compute
            .get_task_connection_info(&instance_id, provider.default_port())
            .await?;

        let params = ConnectionParams {
            host: conn_info.host,
            port: conn_info.port,
            env: conn_info.env,
        };

        // 4. Get schema extraction spec (runs in container, no host tools required).
        let spec = provider
            .schema_extraction_spec(&params)
            .map_err(|e| ExtractSchemaError::ExtractionFailed(e.to_string()))?
            .ok_or_else(|| {
                ExtractSchemaError::ExtractionFailed(format!(
                    "provider '{}' does not support schema extraction",
                    provider_name
                ))
            })?;

        // 5. Run the schema extraction task linked to the database container.
        let output = self
            .compute
            .run_task(&spec.definition, &spec.command, Some(&instance_id))
            .await?;

        if output.exit_code != 0 {
            return Err(ExtractSchemaError::QueryFailed(format!(
                "schema extraction task failed (exit {}): {}",
                output.exit_code, output.stderr
            )));
        }

        // 6. Parse stdout into version, schemas, tables, columns.
        let (version, schemas, tables, columns) =
            parse_schema_output(&output.stdout).map_err(ExtractSchemaError::ExtractionFailed)?;

        // 7. Build metadata.
        let metadata = DatasourceMetadata {
            version,
            driver: provider_name,
            schemas,
            tables,
            columns,
            views: None,
            functions: None,
            indexes: None,
            triggers: None,
            materialized_views: None,
            types: None,
            foreign_tables: None,
            policies: None,
            table_privileges: None,
            column_privileges: None,
            config: None,
            publications: None,
            roles: None,
            extensions: None,
        };

        Ok(SchemaOutput { metadata })
    }
}

/// Parsed schema output from the extraction sidecar.
type ParseSchemaOutput = (String, Vec<Schema>, Vec<Table>, Vec<Column>);

/// Parse the stdout from the schema extraction sidecar.
/// Expected format: delimiter lines followed by content until the next delimiter.
fn parse_schema_output(stdout: &str) -> Result<ParseSchemaOutput, String> {
    let mut version = String::new();
    let mut schemas = Vec::new();
    let mut tables = Vec::new();
    let mut columns = Vec::new();

    let mut current_section: Option<&str> = None;
    let mut current_lines: Vec<&str> = Vec::new();

    for line in stdout.lines() {
        let trimmed = line.trim();
        match trimmed {
            DELIM_VERSION => {
                flush_section(
                    current_section,
                    &current_lines,
                    &mut version,
                    &mut schemas,
                    &mut tables,
                    &mut columns,
                )?;
                current_section = Some("version");
                current_lines.clear();
            }
            DELIM_SCHEMAS => {
                flush_section(
                    current_section,
                    &current_lines,
                    &mut version,
                    &mut schemas,
                    &mut tables,
                    &mut columns,
                )?;
                current_section = Some("schemas");
                current_lines.clear();
            }
            DELIM_TABLES => {
                flush_section(
                    current_section,
                    &current_lines,
                    &mut version,
                    &mut schemas,
                    &mut tables,
                    &mut columns,
                )?;
                current_section = Some("tables");
                current_lines.clear();
            }
            DELIM_COLUMNS => {
                flush_section(
                    current_section,
                    &current_lines,
                    &mut version,
                    &mut schemas,
                    &mut tables,
                    &mut columns,
                )?;
                current_section = Some("columns");
                current_lines.clear();
            }
            _ => {
                if current_section.is_some() {
                    current_lines.push(line);
                }
            }
        }
    }
    flush_section(
        current_section,
        &current_lines,
        &mut version,
        &mut schemas,
        &mut tables,
        &mut columns,
    )?;

    Ok((version, schemas, tables, columns))
}

fn flush_section(
    section: Option<&str>,
    lines: &[&str],
    version: &mut String,
    schemas: &mut Vec<Schema>,
    tables: &mut Vec<Table>,
    columns: &mut Vec<Column>,
) -> Result<(), String> {
    let Some(section) = section else {
        return Ok(());
    };
    let content = lines.join("\n").trim().to_string();
    if content.is_empty() {
        return Ok(());
    }
    match section {
        "version" => {
            *version = content.lines().last().unwrap_or("").trim().to_string();
        }
        "schemas" => {
            let json_str = extract_json(&content)?;
            *schemas = serde_json::from_str(&json_str)
                .map_err(|e| format!("failed to parse schemas: {e}"))?;
        }
        "tables" => {
            let json_str = extract_json(&content)?;
            *tables = serde_json::from_str(&json_str)
                .map_err(|e| format!("failed to parse tables: {e}"))?;
        }
        "columns" => {
            let json_str = extract_json(&content)?;
            *columns = serde_json::from_str(&json_str)
                .map_err(|e| format!("failed to parse columns: {e}"))?;
        }
        _ => {}
    }
    Ok(())
}

fn extract_json(content: &str) -> Result<String, String> {
    let json_str = content
        .lines()
        .skip_while(|line| !line.trim().starts_with('[') && !line.trim().starts_with('{'))
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_string();
    if json_str.is_empty() {
        return Err("no JSON found in section".to_string());
    }
    Ok(json_str)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    use std::path::PathBuf;
    use std::sync::Arc;

    use async_trait::async_trait;
    use tempfile::TempDir;

    use crate::model::config::{EnvironmentConfig, GfsConfig, RuntimeConfig};
    use crate::ports::compute::{
        Compute, ComputeDefinition, ExecOutput, InstanceId, InstanceState, InstanceStatus,
        PortMapping, StartOptions,
    };
    use crate::ports::database_provider::{
        ConnectionParams, DatabaseProvider, DatabaseProviderArg, DatabaseProviderRegistry,
        ProviderError, Result as RegistryResult, SIGTERM, SchemaExtractionSpec, SupportedFeature,
    };

    fn existing_repo_path() -> (TempDir, std::path::PathBuf) {
        let temp = TempDir::new().expect("tempdir");
        let path = temp.path().to_path_buf();
        std::fs::create_dir_all(path.join(".gfs")).expect("create .gfs");
        (temp, path)
    }

    fn write_config(
        path: &std::path::Path,
        env: Option<EnvironmentConfig>,
        runtime: Option<RuntimeConfig>,
    ) {
        let config = GfsConfig {
            mount_point: None,
            version: String::new(),
            description: String::new(),
            user: None,
            environment: env,
            runtime,
        };
        config.save(path).expect("save config");
    }

    #[test]
    fn test_parse_schema_output_full() {
        let stdout = r#"GFS_SCHEMA_VERSION
PostgreSQL 16.0 (Debian 16.0-1.pgdg120+1)
GFS_SCHEMA_SCHEMAS
[{"id":1,"name":"public","owner":"postgres"}]
GFS_SCHEMA_TABLES
[{"id":2,"schema":"public","name":"users","rls_enabled":false,"rls_forced":false,"bytes":0,"size":"0 bytes","live_rows_estimate":0,"dead_rows_estimate":0,"comment":null,"primary_keys":[],"relationships":[]}]
GFS_SCHEMA_COLUMNS
[{"id":"public.users.id","table_id":2,"schema":"public","table":"users","name":"id","ordinal_position":1,"data_type":"int4","format":"int4","is_identity":false,"identity_generation":null,"is_generated":false,"is_nullable":false,"is_updatable":true,"is_unique":false,"check":null,"default_value":null,"enums":[],"comment":null}]"#;

        let (version, schemas, tables, columns) = parse_schema_output(stdout).unwrap();

        assert_eq!(version, "PostgreSQL 16.0 (Debian 16.0-1.pgdg120+1)");
        assert_eq!(schemas.len(), 1);
        assert_eq!(schemas[0].name, "public");
        assert_eq!(tables.len(), 1);
        assert_eq!(tables[0].name, "users");
        assert_eq!(columns.len(), 1);
        assert_eq!(columns[0].name, "id");
        assert_eq!(columns[0].table, "users");
    }

    #[test]
    fn test_parse_schema_output_empty_sections() {
        // Empty tables and columns (just public schema)
        let stdout = r#"GFS_SCHEMA_VERSION
PostgreSQL 16.0
GFS_SCHEMA_SCHEMAS
[{"id":1,"name":"public","owner":"postgres"}]
GFS_SCHEMA_TABLES
[]
GFS_SCHEMA_COLUMNS
[]"#;

        let (version, schemas, tables, columns) = parse_schema_output(stdout).unwrap();

        assert_eq!(version, "PostgreSQL 16.0");
        assert_eq!(schemas.len(), 1);
        assert!(tables.is_empty());
        assert!(columns.is_empty());
    }

    #[test]
    fn test_parse_schema_output_missing_json() {
        // Schemas section has no JSON array
        let stdout = r#"GFS_SCHEMA_VERSION
PostgreSQL 16.0
GFS_SCHEMA_SCHEMAS
some non-json garbage
GFS_SCHEMA_TABLES
[]"#;

        let result = parse_schema_output(stdout);
        let err = result.unwrap_err();
        assert!(
            err.contains("no JSON found") || err.contains("parse schemas"),
            "expected parse error, got: {err}"
        );
    }

    #[test]
    fn test_parse_schema_output_invalid_json() {
        // Schemas section has invalid JSON (not parseable)
        let stdout = r#"GFS_SCHEMA_VERSION
PostgreSQL 16.0
GFS_SCHEMA_SCHEMAS
[invalid json here
GFS_SCHEMA_TABLES
[]"#;

        let result = parse_schema_output(stdout);
        assert!(result.is_err());
    }

    // -----------------------------------------------------------------------
    // Mock Compute for ExtractSchemaUseCase
    // -----------------------------------------------------------------------

    #[derive(Default)]
    struct SchemaExtractMockCompute {
        run_task_stdout: String,
        run_task_exit_code: i32,
    }

    #[async_trait]
    impl Compute for SchemaExtractMockCompute {
        async fn provision(
            &self,
            _: &ComputeDefinition,
        ) -> crate::ports::compute::Result<InstanceId> {
            Ok(InstanceId("mock".into()))
        }
        async fn start(
            &self,
            id: &InstanceId,
            _: StartOptions,
        ) -> crate::ports::compute::Result<InstanceStatus> {
            Ok(InstanceStatus {
                id: id.clone(),
                state: InstanceState::Running,
                pid: None,
                started_at: None,
                exit_code: None,
            })
        }
        async fn stop(&self, id: &InstanceId) -> crate::ports::compute::Result<InstanceStatus> {
            Ok(InstanceStatus {
                id: id.clone(),
                state: InstanceState::Stopped,
                pid: None,
                started_at: None,
                exit_code: None,
            })
        }
        async fn restart(&self, id: &InstanceId) -> crate::ports::compute::Result<InstanceStatus> {
            Ok(InstanceStatus {
                id: id.clone(),
                state: InstanceState::Running,
                pid: None,
                started_at: None,
                exit_code: None,
            })
        }
        async fn status(&self, id: &InstanceId) -> crate::ports::compute::Result<InstanceStatus> {
            Ok(InstanceStatus {
                id: id.clone(),
                state: InstanceState::Running,
                pid: None,
                started_at: None,
                exit_code: None,
            })
        }
        async fn prepare_for_snapshot(
            &self,
            _: &InstanceId,
            _: &[String],
        ) -> crate::ports::compute::Result<()> {
            Ok(())
        }
        async fn logs(
            &self,
            _: &InstanceId,
            _: crate::ports::compute::LogsOptions,
        ) -> crate::ports::compute::Result<Vec<crate::ports::compute::LogEntry>> {
            Ok(vec![])
        }
        async fn pause(&self, id: &InstanceId) -> crate::ports::compute::Result<InstanceStatus> {
            Ok(InstanceStatus {
                id: id.clone(),
                state: InstanceState::Paused,
                pid: None,
                started_at: None,
                exit_code: None,
            })
        }
        async fn unpause(&self, id: &InstanceId) -> crate::ports::compute::Result<InstanceStatus> {
            Ok(InstanceStatus {
                id: id.clone(),
                state: InstanceState::Running,
                pid: None,
                started_at: None,
                exit_code: None,
            })
        }
        async fn get_connection_info(
            &self,
            _: &InstanceId,
            port: u16,
        ) -> crate::ports::compute::Result<crate::ports::compute::InstanceConnectionInfo> {
            Ok(crate::ports::compute::InstanceConnectionInfo {
                host: "127.0.0.1".into(),
                port,
                env: vec![],
            })
        }
        async fn get_instance_data_mount_host_path(
            &self,
            _: &InstanceId,
            _: &str,
        ) -> crate::ports::compute::Result<Option<PathBuf>> {
            Ok(None)
        }
        async fn remove_instance(&self, _: &InstanceId) -> crate::ports::compute::Result<()> {
            Ok(())
        }
        async fn get_task_connection_info(
            &self,
            _: &InstanceId,
            port: u16,
        ) -> crate::ports::compute::Result<crate::ports::compute::InstanceConnectionInfo> {
            Ok(crate::ports::compute::InstanceConnectionInfo {
                host: "172.17.0.2".into(),
                port,
                env: vec![],
            })
        }
        async fn run_task(
            &self,
            _: &ComputeDefinition,
            _: &str,
            _: Option<&InstanceId>,
        ) -> crate::ports::compute::Result<ExecOutput> {
            Ok(ExecOutput {
                exit_code: self.run_task_exit_code,
                stdout: self.run_task_stdout.clone(),
                stderr: "task failed".into(),
            })
        }
    }

    // -----------------------------------------------------------------------
    // Mock provider with schema_extraction_spec
    // -----------------------------------------------------------------------

    struct MockSchemaProvider {
        schema_spec: Option<SchemaExtractionSpec>,
    }

    impl DatabaseProvider for MockSchemaProvider {
        fn name(&self) -> &str {
            "mock-schema"
        }
        fn definition(&self) -> ComputeDefinition {
            ComputeDefinition {
                image: "mock:latest".into(),
                env: vec![],
                ports: vec![PortMapping {
                    compute_port: 5432,
                    host_port: None,
                }],
                data_dir: PathBuf::from("/data"),
                host_data_dir: None,
                logs_dir: None,
                conf_dir: None,
                args: vec![],
            }
        }
        fn default_port(&self) -> u16 {
            5432
        }
        fn default_args(&self) -> Vec<DatabaseProviderArg> {
            vec![]
        }
        fn default_signal(&self) -> u32 {
            SIGTERM
        }
        fn connection_string(
            &self,
            _: &ConnectionParams,
        ) -> std::result::Result<String, ProviderError> {
            Ok("mock://localhost:5432".into())
        }
        fn supported_versions(&self) -> Vec<String> {
            vec!["latest".to_string()]
        }
        fn supported_features(&self) -> Vec<SupportedFeature> {
            vec![SupportedFeature {
                id: "schema".into(),
                description: "Schema support.".into(),
            }]
        }
        fn prepare_for_snapshot(&self, _: &ConnectionParams) -> RegistryResult<Vec<String>> {
            Ok(vec![])
        }
        fn query_client_command(
            &self,
            _: &ConnectionParams,
            _: Option<&str>,
        ) -> std::result::Result<std::process::Command, ProviderError> {
            Ok(std::process::Command::new("true"))
        }
        fn schema_extraction_spec(
            &self,
            _params: &ConnectionParams,
        ) -> std::result::Result<Option<SchemaExtractionSpec>, ProviderError> {
            if let Some(ref spec) = self.schema_spec {
                Ok(Some(spec.clone()))
            } else {
                Ok(None)
            }
        }
    }

    struct MockSchemaRegistry {
        provider: Option<Arc<MockSchemaProvider>>,
    }

    impl DatabaseProviderRegistry for MockSchemaRegistry {
        fn register(&self, _: Arc<dyn DatabaseProvider>) -> RegistryResult<()> {
            Ok(())
        }
        fn get(&self, name: &str) -> Option<Arc<dyn DatabaseProvider>> {
            if name == "mock-schema" {
                self.provider
                    .as_ref()
                    .map(|p| Arc::clone(p) as Arc<dyn DatabaseProvider>)
            } else {
                None
            }
        }
        fn list(&self) -> Vec<String> {
            if self.provider.is_some() {
                vec!["mock-schema".into()]
            } else {
                vec![]
            }
        }
        fn unregister(&self, _: &str) -> Option<Arc<dyn DatabaseProvider>> {
            None
        }
    }

    #[tokio::test]
    async fn test_extract_schema_not_configured() {
        let (_temp, path) = existing_repo_path();
        write_config(&path, None, None);

        let sample_stdout = r#"GFS_SCHEMA_VERSION
PostgreSQL 16.0
GFS_SCHEMA_SCHEMAS
[{"id":1,"name":"public","owner":"postgres"}]
GFS_SCHEMA_TABLES
[]
GFS_SCHEMA_COLUMNS
[]"#;

        let compute = Arc::new(SchemaExtractMockCompute {
            run_task_stdout: sample_stdout.into(),
            run_task_exit_code: 0,
        });
        let provider = MockSchemaProvider {
            schema_spec: Some(SchemaExtractionSpec {
                definition: ComputeDefinition {
                    image: "postgres:latest".into(),
                    env: vec![],
                    ports: vec![],
                    data_dir: PathBuf::from("/tmp"),
                    host_data_dir: None,
                    logs_dir: None,
                    conf_dir: None,
                    args: vec![],
                },
                command: "echo test".into(),
            }),
        };
        let registry = Arc::new(MockSchemaRegistry {
            provider: Some(Arc::new(provider)),
        });

        let use_case = ExtractSchemaUseCase::new(compute, registry);
        let result = use_case.run(&path).await;

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            err.to_string().contains("no container configured")
                || err.to_string().contains("no database provider configured")
                || err.to_string().contains("NotConfigured"),
            "expected NotConfigured, got: {err}"
        );
    }

    #[tokio::test]
    async fn test_extract_schema_provider_not_found() {
        let (_temp, path) = existing_repo_path();
        write_config(
            &path,
            Some(EnvironmentConfig {
                database_provider: "unknown-provider".into(),
                database_version: "17".into(),
            }),
            Some(RuntimeConfig {
                runtime_provider: "docker".into(),
                runtime_version: "24".into(),
                container_name: "gfs-postgres-123".into(),
            }),
        );

        let compute = Arc::new(SchemaExtractMockCompute {
            run_task_stdout: String::new(),
            run_task_exit_code: 0,
        });
        let registry = Arc::new(MockSchemaRegistry { provider: None });

        let use_case = ExtractSchemaUseCase::new(compute, registry);
        let result = use_case.run(&path).await;

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            err.to_string().contains("unknown-provider")
                || err.to_string().contains("ProviderNotFound")
        );
    }

    #[tokio::test]
    async fn test_extract_schema_provider_no_spec() {
        let (_temp, path) = existing_repo_path();
        write_config(
            &path,
            Some(EnvironmentConfig {
                database_provider: "mock-schema".into(),
                database_version: "17".into(),
            }),
            Some(RuntimeConfig {
                runtime_provider: "docker".into(),
                runtime_version: "24".into(),
                container_name: "gfs-postgres-123".into(),
            }),
        );

        let compute = Arc::new(SchemaExtractMockCompute {
            run_task_stdout: String::new(),
            run_task_exit_code: 0,
        });
        let provider = MockSchemaProvider {
            schema_spec: None, // provider does not support schema extraction
        };
        let registry = Arc::new(MockSchemaRegistry {
            provider: Some(Arc::new(provider)),
        });

        let use_case = ExtractSchemaUseCase::new(compute, registry);
        let result = use_case.run(&path).await;

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            err.to_string()
                .contains("does not support schema extraction"),
            "expected ExtractionFailed, got: {err}"
        );
    }

    #[tokio::test]
    async fn test_extract_schema_success() {
        let (_temp, path) = existing_repo_path();
        write_config(
            &path,
            Some(EnvironmentConfig {
                database_provider: "mock-schema".into(),
                database_version: "17".into(),
            }),
            Some(RuntimeConfig {
                runtime_provider: "docker".into(),
                runtime_version: "24".into(),
                container_name: "gfs-postgres-123".into(),
            }),
        );

        let sample_stdout = r#"GFS_SCHEMA_VERSION
PostgreSQL 16.0
GFS_SCHEMA_SCHEMAS
[{"id":1,"name":"public","owner":"postgres"}]
GFS_SCHEMA_TABLES
[{"id":2,"schema":"public","name":"users","rls_enabled":false,"rls_forced":false,"bytes":0,"size":"0 bytes","live_rows_estimate":0,"dead_rows_estimate":0,"comment":null,"primary_keys":[],"relationships":[]}]
GFS_SCHEMA_COLUMNS
[{"id":"public.users.id","table_id":2,"schema":"public","table":"users","name":"id","ordinal_position":1,"data_type":"int4","format":"int4","is_identity":false,"identity_generation":null,"is_generated":false,"is_nullable":false,"is_updatable":true,"is_unique":false,"check":null,"default_value":null,"enums":[],"comment":null}]"#;

        let compute = Arc::new(SchemaExtractMockCompute {
            run_task_stdout: sample_stdout.into(),
            run_task_exit_code: 0,
        });
        let provider = MockSchemaProvider {
            schema_spec: Some(SchemaExtractionSpec {
                definition: ComputeDefinition {
                    image: "postgres:latest".into(),
                    env: vec![],
                    ports: vec![],
                    data_dir: PathBuf::from("/tmp"),
                    host_data_dir: None,
                    logs_dir: None,
                    conf_dir: None,
                    args: vec![],
                },
                command: "echo test".into(),
            }),
        };
        let registry = Arc::new(MockSchemaRegistry {
            provider: Some(Arc::new(provider)),
        });

        let use_case = ExtractSchemaUseCase::new(compute, registry);
        let result = use_case.run(&path).await;

        let output = result.expect("extract should succeed");
        assert_eq!(output.metadata.version, "PostgreSQL 16.0");
        assert_eq!(output.metadata.driver, "mock-schema");
        assert_eq!(output.metadata.schemas.len(), 1);
        assert_eq!(output.metadata.schemas[0].name, "public");
        assert_eq!(output.metadata.tables.len(), 1);
        assert_eq!(output.metadata.tables[0].name, "users");
        assert_eq!(output.metadata.columns.len(), 1);
        assert_eq!(output.metadata.columns[0].name, "id");
        assert_eq!(output.metadata.columns[0].table, "users");
    }

    #[tokio::test]
    async fn test_extract_schema_query_failed() {
        let (_temp, path) = existing_repo_path();
        write_config(
            &path,
            Some(EnvironmentConfig {
                database_provider: "mock-schema".into(),
                database_version: "17".into(),
            }),
            Some(RuntimeConfig {
                runtime_provider: "docker".into(),
                runtime_version: "24".into(),
                container_name: "gfs-postgres-123".into(),
            }),
        );

        let compute = Arc::new(SchemaExtractMockCompute {
            run_task_stdout: String::new(),
            run_task_exit_code: 1,
        });
        let provider = MockSchemaProvider {
            schema_spec: Some(SchemaExtractionSpec {
                definition: ComputeDefinition {
                    image: "postgres:latest".into(),
                    env: vec![],
                    ports: vec![],
                    data_dir: PathBuf::from("/tmp"),
                    host_data_dir: None,
                    logs_dir: None,
                    conf_dir: None,
                    args: vec![],
                },
                command: "echo test".into(),
            }),
        };
        let registry = Arc::new(MockSchemaRegistry {
            provider: Some(Arc::new(provider)),
        });

        let use_case = ExtractSchemaUseCase::new(compute, registry);
        let result = use_case.run(&path).await;

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            err.to_string().contains("QueryFailed") || err.to_string().contains("exit"),
            "expected QueryFailed, got: {err}"
        );
    }

    #[tokio::test]
    async fn test_extract_schema_config_error() {
        let temp = TempDir::new().expect("tempdir");
        let path = temp.path(); // No .gfs directory

        let compute = Arc::new(SchemaExtractMockCompute::default());
        let registry = Arc::new(MockSchemaRegistry { provider: None });

        let use_case = ExtractSchemaUseCase::new(compute, registry);
        let result = use_case.run(path).await;

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            err.to_string().contains("Config") || err.to_string().contains("config"),
            "expected Config error, got: {err}"
        );
    }
}
