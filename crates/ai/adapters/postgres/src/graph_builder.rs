//! Database to graph conversion utilities.
//!
//! Builds DataGraph instances from database metadata and row data for training
//! GraphSemanticEngine models.

use std::collections::HashMap;
use std::sync::Arc;

use gfs_ai_models::features::extractors::DefaultFeatureExtractor;
use gfs_ai_models::features::FeatureExtractor;
use gfs_ai_models::graph::{DataGraph, DataGraphBuilder};
use gfs_domain::model::datasource::{DatasourceMetadata, Table};
use gfs_domain::ports::compute::Compute;
use gfs_domain::ports::database_provider::{ConnectionParams, DatabaseProvider};
use gfs_domain::ports::query_executor::{QueryExecutor, QueryExecutorError};

/// Configuration for graph building from database.
#[derive(Clone)]
pub struct GraphBuilderConfig {
    /// Which tables to include (None = all public schema tables)
    pub table_filter: Option<Vec<String>>,
    /// Max rows per table (None = all rows)
    pub row_limit: Option<usize>,
    /// Whether to include FK edges
    pub include_relationships: bool,
    /// Feature extraction configuration
    pub feature_extractor: Arc<dyn FeatureExtractor>,
}

impl Default for GraphBuilderConfig {
    fn default() -> Self {
        Self {
            table_filter: None,
            row_limit: None,
            include_relationships: true,
            feature_extractor: Arc::new(DefaultFeatureExtractor::new()),
        }
    }
}

impl GraphBuilderConfig {
    /// Create a new config with default settings.
    pub fn new() -> Self {
        Self::default()
    }

    /// Set table filter (only include these tables).
    /// If the vector is empty, no filter is applied (all tables included).
    pub fn with_table_filter(mut self, tables: Vec<String>) -> Self {
        if !tables.is_empty() {
            self.table_filter = Some(tables);
        }
        self
    }

    /// Set row limit per table.
    pub fn with_row_limit(mut self, limit: usize) -> Self {
        self.row_limit = Some(limit);
        self
    }

    /// Set whether to include FK relationships as edges.
    pub fn with_relationships(mut self, include: bool) -> Self {
        self.include_relationships = include;
        self
    }

    /// Set feature extractor.
    pub fn with_feature_extractor(mut self, extractor: Arc<dyn FeatureExtractor>) -> Self {
        self.feature_extractor = extractor;
        self
    }
}

/// Builder for converting database metadata + data into a DataGraph.
///
/// Extracts rows from tables, converts them to text features, and creates
/// nodes with provenance metadata. Foreign key relationships become edges.
pub struct DatabaseGraphBuilder {
    _provider: Arc<dyn DatabaseProvider>,
    _compute: Arc<dyn Compute>,
    query_executor: Arc<dyn QueryExecutor>,
    config: GraphBuilderConfig,
}

impl DatabaseGraphBuilder {
    /// Create a new database graph builder.
    ///
    /// # Arguments
    ///
    /// * `provider` - Database provider for connection info
    /// * `compute` - Compute abstraction
    /// * `query_executor` - Query executor for fetching data
    /// * `config` - Graph building configuration
    pub fn new(
        provider: Arc<dyn DatabaseProvider>,
        compute: Arc<dyn Compute>,
        query_executor: Arc<dyn QueryExecutor>,
        config: GraphBuilderConfig,
    ) -> Self {
        Self {
            _provider: provider,
            _compute: compute,
            query_executor,
            config,
        }
    }

    /// Build a DataGraph from database metadata and data.
    ///
    /// Steps:
    /// 1. Filter tables based on config
    /// 2. For each table, fetch row data
    /// 3. For each row, extract features → create DataNode
    /// 4. Extract FK relationships → add edges
    ///
    /// # Errors
    ///
    /// Returns error if query execution fails or data parsing fails.
    pub async fn build_graph(
        &self,
        metadata: &DatasourceMetadata,
        connection_params: &ConnectionParams,
    ) -> Result<DataGraph, QueryExecutorError> {
        let mut builder = DataGraph::builder();
        let mut node_counter: usize = 0;

        // Map (schema, table, pk_value) → node_id for FK resolution
        let mut row_to_node: HashMap<(String, String, String), usize> = HashMap::new();

        // 1. Filter tables
        let tables: Vec<&Table> = metadata
            .get_all_tables()
            .into_iter()
            .filter(|t| self.should_include_table(t))
            .collect();

        tracing::info!("Building graph from {} tables", tables.len());

        // 2. For each table, fetch rows and create nodes
        for table in &tables {
            tracing::debug!("Processing table {}.{}", table.schema, table.name);

            let result = self
                .query_executor
                .fetch_table_data(table, connection_params, self.config.row_limit)
                .await?;

            tracing::debug!(
                "Fetched {} rows from {}.{}",
                result.rows.len(),
                table.schema,
                table.name
            );

            // Get columns for feature extraction
            let columns = table
                .columns
                .as_ref()
                .ok_or_else(|| {
                    QueryExecutorError::InvalidQuery(format!(
                        "table {}.{} has no columns metadata",
                        table.schema, table.name
                    ))
                })?;

            // Identify primary key column(s)
            let pk_columns: Vec<String> = table
                .primary_keys
                .iter()
                .map(|pk| pk.name.clone())
                .collect();

            // Create a node for each row
            for row in &result.rows {
                // Extract text features
                let text = self.config.feature_extractor.extract_node_text(row, columns);

                // Skip empty text (all NULLs or stop words)
                if text.trim().is_empty() {
                    continue;
                }

                // Extract primary key value for provenance
                let pk_value = if !pk_columns.is_empty() {
                    self.extract_pk_value(row, &pk_columns)
                } else {
                    // If no PK, use row index as identifier
                    node_counter.to_string()
                };

                // Add node with provenance metadata
                builder = builder.add_node_with_meta(
                    node_counter,
                    text,
                    format!("{}.{}", table.schema, table.name),
                    &pk_value,
                );

                // Record mapping for FK resolution
                row_to_node.insert(
                    (table.schema.clone(), table.name.clone(), pk_value),
                    node_counter,
                );

                node_counter += 1;
            }
        }

        tracing::info!("Created {} nodes", node_counter);

        // 3. Add FK edges if enabled
        let edge_count = if self.config.include_relationships {
            let (new_builder, count) = self.add_fk_edges(builder, &tables, &row_to_node);
            builder = new_builder;
            tracing::info!("Created {} edges from FK relationships", count);
            count
        } else {
            0
        };

        let _ = edge_count; // Silence unused variable warning

        Ok(builder.build())
    }

    /// Check if a table should be included based on config.
    fn should_include_table(&self, table: &Table) -> bool {
        // Skip system schemas
        if table.schema == "pg_catalog"
            || table.schema == "information_schema"
            || table.schema.starts_with("pg_")
        {
            return false;
        }

        // Apply table filter if configured
        if let Some(ref filter) = self.config.table_filter {
            let qualified_name = format!("{}.{}", table.schema, table.name);
            filter.contains(&table.name) || filter.contains(&qualified_name)
        } else {
            true
        }
    }

    /// Extract primary key value from a row.
    ///
    /// If there are multiple PK columns, concatenates them with "_".
    fn extract_pk_value(
        &self,
        row: &HashMap<String, serde_json::Value>,
        pk_columns: &[String],
    ) -> String {
        pk_columns
            .iter()
            .filter_map(|col| {
                row.get(col).and_then(|v| match v {
                    serde_json::Value::Null => None,
                    serde_json::Value::String(s) => Some(s.clone()),
                    serde_json::Value::Number(n) => Some(n.to_string()),
                    _ => Some(v.to_string()),
                })
            })
            .collect::<Vec<_>>()
            .join("_")
    }

    /// Add foreign key relationships as edges.
    ///
    /// Returns a DataGraphBuilder with edges added.
    fn add_fk_edges(
        &self,
        mut builder: DataGraphBuilder,
        tables: &[&Table],
        row_to_node: &HashMap<(String, String, String), usize>,
    ) -> (DataGraphBuilder, usize) {
        let mut edge_count = 0;

        for table in tables {
            for rel in &table.relationships {
                // For each FK relationship, find matching nodes
                // This is simplified: in practice, we'd need to query
                // the actual FK values from rows, but for now we just
                // create edges between tables where rows exist

                // Get all source nodes for this table
                let source_key = (rel.source_schema.clone(), rel.source_table_name.clone());
                // Get all target nodes for this table
                let target_key = (
                    rel.target_table_schema.clone(),
                    rel.target_table_name.clone(),
                );

                // Create edges for matching FK values
                // (This is a simplified approach - proper implementation would
                // need to query actual FK column values)
                for ((schema, tbl, _pk), &source_node) in row_to_node.iter() {
                    if (schema.as_str(), tbl.as_str()) == (source_key.0.as_str(), source_key.1.as_str()) {
                        // For now, create edges to all target table nodes
                        // A proper implementation would match FK values to PKs
                        for ((tgt_schema, tgt_tbl, _), &target_node) in row_to_node.iter() {
                            if (tgt_schema.as_str(), tgt_tbl.as_str())
                                == (target_key.0.as_str(), target_key.1.as_str())
                            {
                                builder = builder.add_edge(source_node, target_node);
                                edge_count += 1;
                            }
                        }
                    }
                }
            }
        }

        (builder, edge_count)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gfs_ai_models::features::extractors::DefaultFeatureExtractor;

    #[test]
    fn test_config_builder() {
        let config = GraphBuilderConfig::new()
            .with_table_filter(vec!["users".to_string(), "products".to_string()])
            .with_row_limit(100)
            .with_relationships(true);

        assert_eq!(
            config.table_filter,
            Some(vec!["users".to_string(), "products".to_string()])
        );
        assert_eq!(config.row_limit, Some(100));
        assert!(config.include_relationships);
    }

    #[test]
    fn test_config_with_custom_extractor() {
        let extractor = Arc::new(DefaultFeatureExtractor::new().with_separator(" | "));
        let config = GraphBuilderConfig::new().with_feature_extractor(extractor);

        // Verify extractor is set (can't easily test Arc equality, so just verify it doesn't panic)
        assert!(config.feature_extractor.extract_node_text(
            &HashMap::new(),
            &[]
        ).is_empty());
    }

    #[test]
    fn test_extract_pk_value_single() {
        let builder = create_test_builder();
        let mut row = HashMap::new();
        row.insert("id".to_string(), serde_json::json!(42));

        let pk = builder.extract_pk_value(&row, &["id".to_string()]);
        assert_eq!(pk, "42");
    }

    #[test]
    fn test_extract_pk_value_composite() {
        let builder = create_test_builder();
        let mut row = HashMap::new();
        row.insert("user_id".to_string(), serde_json::json!(1));
        row.insert("post_id".to_string(), serde_json::json!(100));

        let pk = builder.extract_pk_value(&row, &["user_id".to_string(), "post_id".to_string()]);
        assert_eq!(pk, "1_100");
    }

    #[test]
    fn test_extract_pk_value_string() {
        let builder = create_test_builder();
        let mut row = HashMap::new();
        row.insert("uuid".to_string(), serde_json::json!("abc-123"));

        let pk = builder.extract_pk_value(&row, &["uuid".to_string()]);
        assert_eq!(pk, "abc-123");
    }

    #[test]
    fn test_extract_pk_value_null_skipped() {
        let builder = create_test_builder();
        let mut row = HashMap::new();
        row.insert("id1".to_string(), serde_json::json!(1));
        row.insert("id2".to_string(), serde_json::Value::Null);

        let pk = builder.extract_pk_value(&row, &["id1".to_string(), "id2".to_string()]);
        assert_eq!(pk, "1");
    }

    // Helper to create a test builder
    fn create_test_builder() -> DatabaseGraphBuilder {
        use gfs_domain::ports::compute::Compute;
        use gfs_domain::ports::database_provider::DatabaseProvider;
        use gfs_domain::ports::query_executor::QueryExecutor;

        // Use the mock implementations from query_executor tests
        struct MockProvider;
        impl DatabaseProvider for MockProvider {
            fn name(&self) -> &str {
                "mock"
            }
            fn definition(&self) -> gfs_domain::ports::compute::ComputeDefinition {
                gfs_domain::ports::compute::ComputeDefinition {
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
            fn default_args(
                &self,
            ) -> Vec<gfs_domain::ports::database_provider::DatabaseProviderArg> {
                vec![]
            }
            fn connection_string(
                &self,
                _: &ConnectionParams,
            ) -> std::result::Result<String, gfs_domain::ports::database_provider::ProviderError>
            {
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
            ) -> std::result::Result<
                std::process::Command,
                gfs_domain::ports::database_provider::ProviderError,
            > {
                Ok(std::process::Command::new("true"))
            }
        }

        struct MockCompute;
        #[async_trait]
        impl Compute for MockCompute {
            async fn provision(
                &self,
                _: &gfs_domain::ports::compute::InstanceId,
                _: &gfs_domain::ports::compute::ComputeDefinition,
            ) -> gfs_domain::ports::compute::Result<()> {
                Ok(())
            }
            async fn start(
                &self,
                _: &gfs_domain::ports::compute::InstanceId,
            ) -> gfs_domain::ports::compute::Result<()> {
                Ok(())
            }
            async fn stop(
                &self,
                _: &gfs_domain::ports::compute::InstanceId,
                _: u32,
            ) -> gfs_domain::ports::compute::Result<()> {
                Ok(())
            }
            async fn remove(
                &self,
                _: &gfs_domain::ports::compute::InstanceId,
            ) -> gfs_domain::ports::compute::Result<()> {
                Ok(())
            }
            async fn get_instance_info(
                &self,
                _: &gfs_domain::ports::compute::InstanceId,
            ) -> gfs_domain::ports::compute::Result<gfs_domain::ports::compute::InstanceInfo> {
                Ok(gfs_domain::ports::compute::InstanceInfo {
                    id: gfs_domain::ports::compute::InstanceId("mock".into()),
                    status: gfs_domain::ports::compute::InstanceStatus::Running,
                })
            }
            async fn get_connection_info(
                &self,
                _: &gfs_domain::ports::compute::InstanceId,
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
                _: &gfs_domain::ports::compute::InstanceId,
                _: &gfs_domain::ports::compute::SnapshotId,
            ) -> gfs_domain::ports::compute::Result<()> {
                Ok(())
            }
            async fn restore_snapshot(
                &self,
                _: &gfs_domain::ports::compute::InstanceId,
                _: &gfs_domain::ports::compute::SnapshotId,
            ) -> gfs_domain::ports::compute::Result<()> {
                Ok(())
            }
            async fn delete_snapshot(
                &self,
                _: &gfs_domain::ports::compute::SnapshotId,
            ) -> gfs_domain::ports::compute::Result<()> {
                Ok(())
            }
            async fn list_snapshots(
                &self,
                _: &gfs_domain::ports::compute::InstanceId,
            ) -> gfs_domain::ports::compute::Result<Vec<gfs_domain::ports::compute::SnapshotInfo>>
            {
                Ok(vec![])
            }
        }

        struct MockQueryExecutor;
        #[async_trait]
        impl QueryExecutor for MockQueryExecutor {
            async fn execute_query(
                &self,
                _: &str,
                _: &ConnectionParams,
            ) -> gfs_domain::ports::query_executor::Result<
                gfs_domain::model::datasource::DatasourceResultSet,
            > {
                Ok(gfs_domain::model::datasource::DatasourceResultSet {
                    rows: vec![],
                    columns: vec![],
                    stat: gfs_domain::model::datasource::DatasourceResultStat::default(),
                })
            }
        }

        DatabaseGraphBuilder::new(
            Arc::new(MockProvider),
            Arc::new(MockCompute),
            Arc::new(MockQueryExecutor),
            GraphBuilderConfig::default(),
        )
    }
}
