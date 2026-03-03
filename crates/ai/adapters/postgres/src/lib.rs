//! Postgres adapter for AI training infrastructure.
//!
//! Provides query execution and graph building capabilities for extracting
//! training data from PostgreSQL databases.

pub mod query_executor;
pub mod graph_builder;

pub use query_executor::PostgresQueryExecutor;
pub use graph_builder::{DatabaseGraphBuilder, GraphBuilderConfig};
