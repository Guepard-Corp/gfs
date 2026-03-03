//! Feature extraction for converting database rows into text representations.
//!
//! This module provides utilities for transforming structured database data
//! (rows with typed columns) into text strings suitable for embedding with
//! Word2Vec and graph neural networks.

pub mod extractors;

use gfs_domain::model::datasource::{Column, DatasourceRow};

/// Trait for extracting text features from database rows.
///
/// Feature extractors convert structured data (column values) into text
/// strings that can be embedded by the GraphSemanticEngine. Implementations
/// handle different column types, NULL values, and text normalization.
pub trait FeatureExtractor: Send + Sync {
    /// Extract text representation from a database row.
    ///
    /// Converts the row's column values into a single text string. The
    /// implementation determines how to combine values, handle NULLs,
    /// and format different data types.
    ///
    /// # Arguments
    ///
    /// * `row` - The database row to extract features from
    /// * `columns` - Column metadata describing the row's schema
    ///
    /// # Returns
    ///
    /// A text string suitable for embedding. May be empty if the row
    /// contains only NULL values or stop words.
    fn extract_node_text(&self, row: &DatasourceRow, columns: &[Column]) -> String;

    /// Generate text representation for a NULL value.
    ///
    /// Determines how to represent missing data in the text output.
    /// Common strategies:
    /// - Return empty string (skip NULL values)
    /// - Return "null" or "missing" (explicit representation)
    /// - Return column name with "_null" suffix
    ///
    /// # Arguments
    ///
    /// * `column` - The column metadata for context
    ///
    /// # Returns
    ///
    /// Text representation of NULL for this column
    fn handle_null(&self, column: &Column) -> String;

    /// Encode a categorical value into text.
    ///
    /// Provides a hook for custom encoding of categorical data (e.g.,
    /// one-hot encoding, label encoding). Default implementation returns
    /// the value as-is.
    ///
    /// # Arguments
    ///
    /// * `value` - The categorical value
    /// * `column` - The column metadata for context
    ///
    /// # Returns
    ///
    /// Text encoding of the categorical value
    fn encode_categorical(&self, value: &str, column: &Column) -> String {
        let _ = column; // unused in default implementation
        value.to_string()
    }
}
