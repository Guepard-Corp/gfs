//! Postgres query executor implementation.
//!
//! Executes SQL queries against PostgreSQL databases and parses results into
//! DatasourceResultSet for use by training infrastructure.

use std::sync::Arc;

use async_trait::async_trait;
use gfs_domain::model::datasource::{ColumnHeader, ColumnType, DatasourceResultSet, DatasourceResultStat};
use gfs_domain::ports::compute::Compute;
use gfs_domain::ports::database_provider::{ConnectionParams, DatabaseProvider};
use gfs_domain::ports::query_executor::{QueryExecutor, QueryExecutorError, Result};
use serde_json::Value;

/// Postgres-specific query executor.
///
/// Executes queries using the psql command-line client and parses JSON output.
/// Requires psql to be installed on the host system or available in the
/// container.
pub struct PostgresQueryExecutor {
    provider: Arc<dyn DatabaseProvider>,
    _compute: Arc<dyn Compute>,
}

impl PostgresQueryExecutor {
    /// Create a new PostgreSQL query executor.
    ///
    /// # Arguments
    ///
    /// * `provider` - The PostgreSQL database provider for connection info
    /// * `compute` - The compute abstraction (for future container-based execution)
    pub fn new(provider: Arc<dyn DatabaseProvider>, compute: Arc<dyn Compute>) -> Self {
        Self {
            provider,
            _compute: compute,
        }
    }

    /// Parse psql JSON output into a DatasourceResultSet.
    ///
    /// psql with `-t -A` and `\set ON_ERROR_STOP on` produces clean JSON output.
    /// We wrap the query with:
    /// ```sql
    /// SELECT row_to_json(t) FROM (
    ///   <user_query>
    /// ) t
    /// ```
    ///
    /// This produces one JSON object per row, which we collect into an array.
    fn parse_json_output(&self, output: &str) -> Result<DatasourceResultSet> {
        let output = output.trim();
        if output.is_empty() {
            return Ok(DatasourceResultSet {
                rows: vec![],
                columns: vec![],
                stat: DatasourceResultStat::default(),
            });
        }

        // Parse each line as a JSON object
        let mut rows = Vec::new();
        let mut columns_map = std::collections::HashMap::new();
        let mut column_order = Vec::new();

        for line in output.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }

            let row_json: Value = serde_json::from_str(line)
                .map_err(|e| QueryExecutorError::ParseError(format!("invalid JSON: {}", e)))?;

            if let Value::Object(obj) = row_json {
                // Collect column names on first row
                if columns_map.is_empty() {
                    for key in obj.keys() {
                        column_order.push(key.clone());
                        columns_map.insert(key.clone(), infer_column_type(obj.get(key).unwrap()));
                    }
                }

                // Convert JSON object to DatasourceRow (HashMap<String, Value>)
                let mut row = std::collections::HashMap::new();
                for (key, value) in obj {
                    row.insert(key, value);
                }
                rows.push(row);
            } else {
                return Err(QueryExecutorError::ParseError(
                    "expected JSON object per row".to_string(),
                ));
            }
        }

        // Build columns metadata
        let columns: Vec<ColumnHeader> = column_order
            .iter()
            .map(|name| ColumnHeader {
                name: name.clone(),
                display_name: name.clone(),
                original_type: None,
                type_hint: columns_map.get(name).and_then(|&t| t),
                schema: None,
                table: None,
                original_name: Some(name.clone()),
                primary_key: None,
                column_id: None,
                table_id: None,
            })
            .collect();

        let row_count = rows.len() as u64;

        Ok(DatasourceResultSet {
            rows,
            columns,
            stat: DatasourceResultStat {
                rows_affected: row_count,
                rows_read: Some(row_count),
                rows_written: None,
                query_duration_ms: None,
            },
        })
    }

    /// Wrap a user query to produce JSON output.
    fn wrap_query_for_json(&self, query: &str) -> String {
        format!(
            "SELECT row_to_json(t) FROM (\n{}\n) t;",
            query.trim_end_matches(';')
        )
    }
}

/// Infer ColumnType from a JSON value.
fn infer_column_type(value: &Value) -> Option<ColumnType> {
    match value {
        Value::Null => Some(ColumnType::Null),
        Value::Bool(_) => Some(ColumnType::Boolean),
        Value::Number(n) => {
            if n.is_i64() {
                Some(ColumnType::Integer)
            } else {
                Some(ColumnType::Float)
            }
        }
        Value::String(_) => Some(ColumnType::String),
        Value::Array(_) => Some(ColumnType::Array),
        Value::Object(_) => Some(ColumnType::Json),
    }
}

#[async_trait]
impl QueryExecutor for PostgresQueryExecutor {
    async fn execute_query(
        &self,
        query: &str,
        params: &ConnectionParams,
    ) -> Result<DatasourceResultSet> {
        // Wrap query to produce JSON output
        let wrapped_query = self.wrap_query_for_json(query);

        // Build psql command with -t (tuples only, no headers) and -A (unaligned mode)
        let mut cmd = self
            .provider
            .query_client_command(params, Some(&wrapped_query))
            .map_err(|e| QueryExecutorError::ConnectionError(e.to_string()))?;

        // Add flags for clean JSON output
        cmd.arg("-t") // tuples only (no headers/footer)
            .arg("-A") // unaligned mode (no spacing)
            .arg("-q"); // quiet mode (no \timing, etc.)

        // Execute command and capture output
        let output = tokio::task::spawn_blocking(move || cmd.output())
            .await
            .map_err(|e| QueryExecutorError::ExecutionFailed(format!("task join error: {}", e)))?
            .map_err(|e| QueryExecutorError::ExecutionFailed(format!("command failed: {}", e)))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(QueryExecutorError::ExecutionFailed(format!(
                "query failed (exit code {}): {}",
                output.status.code().unwrap_or(-1),
                stderr
            )));
        }

        // Parse stdout as JSON
        let stdout = String::from_utf8_lossy(&output.stdout);
        self.parse_json_output(&stdout)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_wrap_query_for_json() {
        let executor = PostgresQueryExecutor {
            provider: Arc::new(MockProvider),
            _compute: Arc::new(MockCompute),
        };

        let wrapped = executor.wrap_query_for_json("SELECT * FROM users");
        assert!(wrapped.contains("row_to_json"));
        assert!(wrapped.contains("SELECT * FROM users"));
    }

    #[test]
    fn test_infer_column_type() {
        assert_eq!(infer_column_type(&Value::Null), Some(ColumnType::Null));
        assert_eq!(
            infer_column_type(&Value::Bool(true)),
            Some(ColumnType::Boolean)
        );
        assert_eq!(
            infer_column_type(&Value::Number(42.into())),
            Some(ColumnType::Integer)
        );
        assert_eq!(
            infer_column_type(&Value::Number(serde_json::Number::from_f64(3.14).unwrap())),
            Some(ColumnType::Float)
        );
        assert_eq!(
            infer_column_type(&Value::String("hello".into())),
            Some(ColumnType::String)
        );
        assert_eq!(
            infer_column_type(&Value::Array(vec![])),
            Some(ColumnType::Array)
        );
        assert_eq!(
            infer_column_type(&serde_json::json!({"key": "value"})),
            Some(ColumnType::Json)
        );
    }

    #[test]
    fn test_parse_json_output_empty() {
        let executor = PostgresQueryExecutor {
            provider: Arc::new(MockProvider),
            _compute: Arc::new(MockCompute),
        };

        let result = executor.parse_json_output("").unwrap();
        assert_eq!(result.rows.len(), 0);
        assert_eq!(result.columns.len(), 0);
    }

    #[test]
    fn test_parse_json_output_single_row() {
        let executor = PostgresQueryExecutor {
            provider: Arc::new(MockProvider),
            _compute: Arc::new(MockCompute),
        };

        let output = r#"{"id":1,"name":"Alice","age":30}"#;
        let result = executor.parse_json_output(output).unwrap();

        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.columns.len(), 3);
        assert!(result.columns.iter().any(|c| c.name == "id"));
        assert!(result.columns.iter().any(|c| c.name == "name"));
        assert!(result.columns.iter().any(|c| c.name == "age"));
    }

    #[test]
    fn test_parse_json_output_multiple_rows() {
        let executor = PostgresQueryExecutor {
            provider: Arc::new(MockProvider),
            _compute: Arc::new(MockCompute),
        };

        let output = r#"{"id":1,"name":"Alice"}
{"id":2,"name":"Bob"}"#;
        let result = executor.parse_json_output(output).unwrap();

        assert_eq!(result.rows.len(), 2);
        assert_eq!(result.stat.rows_affected, 2);
    }

    // Mock implementations for testing
    use gfs_domain::ports::compute::{
        ComputeDefinition, InstanceId, InstanceInfo, SnapshotId, SnapshotInfo,
    };
    use gfs_domain::ports::database_provider::ProviderError;

    struct MockProvider;

    impl DatabaseProvider for MockProvider {
        fn name(&self) -> &str {
            "mock"
        }
        fn definition(&self) -> ComputeDefinition {
            ComputeDefinition {
                image: "mock:latest".into(),
                env: vec![],
                ports: vec![],
                data_dir: "/data".into(),
                host_data_dir: None,
                user: None,
                logs_dir: None,
                conf_dir: None,
                args: vec![],
            }
        }
        fn default_port(&self) -> u16 {
            5432
        }
        fn default_args(&self) -> Vec<gfs_domain::ports::database_provider::DatabaseProviderArg> {
            vec![]
        }
        fn connection_string(
            &self,
            _: &ConnectionParams,
        ) -> std::result::Result<String, ProviderError> {
            Ok("mock://localhost".into())
        }
        fn supported_versions(&self) -> Vec<String> {
            vec![]
        }
        fn supported_features(
            &self,
        ) -> Vec<gfs_domain::ports::database_provider::SupportedFeature> {
            vec![]
        }
        fn prepare_for_snapshot(
            &self,
            _: &ConnectionParams,
        ) -> gfs_domain::ports::database_provider::Result<Vec<String>> {
            Ok(vec![])
        }
        fn query_client_command(
            &self,
            _: &ConnectionParams,
            _: Option<&str>,
        ) -> std::result::Result<std::process::Command, ProviderError> {
            Ok(std::process::Command::new("true"))
        }
    }

    struct MockCompute;

    #[async_trait]
    impl Compute for MockCompute {
        async fn provision(
            &self,
            _: &InstanceId,
            _: &ComputeDefinition,
        ) -> gfs_domain::ports::compute::Result<()> {
            Ok(())
        }
        async fn start(&self, _: &InstanceId) -> gfs_domain::ports::compute::Result<()> {
            Ok(())
        }
        async fn stop(&self, _: &InstanceId, _: u32) -> gfs_domain::ports::compute::Result<()> {
            Ok(())
        }
        async fn remove(&self, _: &InstanceId) -> gfs_domain::ports::compute::Result<()> {
            Ok(())
        }
        async fn get_instance_info(
            &self,
            _: &InstanceId,
        ) -> gfs_domain::ports::compute::Result<InstanceInfo> {
            Ok(InstanceInfo {
                id: InstanceId("mock".into()),
                status: gfs_domain::ports::compute::InstanceStatus::Running,
            })
        }
        async fn get_connection_info(
            &self,
            _: &InstanceId,
            _: u16,
        ) -> gfs_domain::ports::compute::Result<gfs_domain::ports::compute::ConnectionInfo> {
            Ok(gfs_domain::ports::compute::ConnectionInfo {
                host: "localhost".into(),
                port: 5432,
                env: vec![],
            })
        }
        async fn create_snapshot(
            &self,
            _: &InstanceId,
            _: &SnapshotId,
        ) -> gfs_domain::ports::compute::Result<()> {
            Ok(())
        }
        async fn restore_snapshot(
            &self,
            _: &InstanceId,
            _: &SnapshotId,
        ) -> gfs_domain::ports::compute::Result<()> {
            Ok(())
        }
        async fn delete_snapshot(
            &self,
            _: &SnapshotId,
        ) -> gfs_domain::ports::compute::Result<()> {
            Ok(())
        }
        async fn list_snapshots(
            &self,
            _: &InstanceId,
        ) -> gfs_domain::ports::compute::Result<Vec<SnapshotInfo>> {
            Ok(vec![])
        }
    }
}
