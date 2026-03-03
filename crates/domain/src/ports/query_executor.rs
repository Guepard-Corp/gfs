//! Query execution port for programmatic database operations.
//!
//! Provides an async interface for executing SQL queries and fetching table data
//! that can be used by training infrastructure and other components needing
//! direct database access beyond the CLI.

use async_trait::async_trait;

use crate::model::datasource::{DatasourceResultSet, Table};
use crate::ports::database_provider::ConnectionParams;

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[derive(Debug, thiserror::Error)]
pub enum QueryExecutorError {
    #[error("query execution failed: {0}")]
    ExecutionFailed(String),

    #[error("failed to parse query result: {0}")]
    ParseError(String),

    #[error("connection failed: {0}")]
    ConnectionError(String),

    #[error("timeout executing query")]
    Timeout,

    #[error("invalid query: {0}")]
    InvalidQuery(String),
}

pub type Result<T> = std::result::Result<T, QueryExecutorError>;

// ---------------------------------------------------------------------------
// Port
// ---------------------------------------------------------------------------

/// Port for executing queries against a database.
///
/// Provides programmatic query execution for use cases like training data
/// extraction, data analysis, and semantic search. Implementations handle
/// connection management, query execution, and result parsing.
///
/// # Example
///
/// ```ignore
/// let executor = PostgresQueryExecutor::new(provider, compute);
/// let params = ConnectionParams {
///     host: "localhost".to_string(),
///     port: 5432,
///     env: vec![("POSTGRES_USER".to_string(), "user".to_string())],
/// };
///
/// let result = executor.execute_query(
///     "SELECT * FROM products LIMIT 10",
///     &params,
/// ).await?;
///
/// for row in result.rows {
///     println!("{:?}", row);
/// }
/// ```
#[async_trait]
pub trait QueryExecutor: Send + Sync {
    /// Execute a SQL query and return parsed results.
    ///
    /// The query is executed against the database specified by `params`.
    /// Results are parsed into a `DatasourceResultSet` with typed columns.
    ///
    /// # Errors
    ///
    /// Returns `QueryExecutorError::ExecutionFailed` if the query fails
    /// (syntax error, permission denied, etc.).
    ///
    /// Returns `QueryExecutorError::ParseError` if the output cannot be
    /// parsed into a `DatasourceResultSet`.
    ///
    /// Returns `QueryExecutorError::ConnectionError` if connection fails.
    async fn execute_query(
        &self,
        query: &str,
        params: &ConnectionParams,
    ) -> Result<DatasourceResultSet>;

    /// Fetch all rows from a table, optionally limiting the number of rows.
    ///
    /// Convenience method for `SELECT * FROM table LIMIT n`. The `limit`
    /// parameter controls how many rows to fetch (None = all rows).
    ///
    /// # Errors
    ///
    /// Same as `execute_query`.
    async fn fetch_table_data(
        &self,
        table: &Table,
        params: &ConnectionParams,
        limit: Option<usize>,
    ) -> Result<DatasourceResultSet> {
        let limit_clause = limit
            .map(|n| format!(" LIMIT {}", n))
            .unwrap_or_default();

        let query = format!(
            "SELECT * FROM \"{}\".\"{}\"{}",
            table.schema, table.name, limit_clause
        );

        self.execute_query(&query, params).await
    }

    /// Execute multiple queries in batch.
    ///
    /// Executes queries sequentially and returns results in the same order.
    /// If any query fails, execution stops and an error is returned.
    ///
    /// # Errors
    ///
    /// Same as `execute_query`. On error, results for successfully executed
    /// queries are discarded.
    async fn execute_batch(
        &self,
        queries: &[String],
        params: &ConnectionParams,
    ) -> Result<Vec<DatasourceResultSet>> {
        let mut results = Vec::with_capacity(queries.len());
        for query in queries {
            let result = self.execute_query(query, params).await?;
            results.push(result);
        }
        Ok(results)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_query_executor_error_display() {
        let err = QueryExecutorError::ExecutionFailed("syntax error".to_string());
        assert_eq!(err.to_string(), "query execution failed: syntax error");

        let err = QueryExecutorError::ParseError("invalid json".to_string());
        assert_eq!(err.to_string(), "failed to parse query result: invalid json");

        let err = QueryExecutorError::ConnectionError("host unreachable".to_string());
        assert_eq!(err.to_string(), "connection failed: host unreachable");

        let err = QueryExecutorError::Timeout;
        assert_eq!(err.to_string(), "timeout executing query");

        let err = QueryExecutorError::InvalidQuery("missing FROM".to_string());
        assert_eq!(err.to_string(), "invalid query: missing FROM");
    }
}
