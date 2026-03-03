//! Default feature extractor implementations.

use gfs_domain::model::datasource::{Column, DatasourceRow};
use serde_json::Value;

use super::FeatureExtractor;

/// Default feature extractor that concatenates column values with a separator.
///
/// This implementation provides sensible defaults for text feature extraction:
/// - Concatenates non-NULL column values with a configurable separator
/// - Optionally includes column names as prefixes (e.g., "name: John age: 30")
/// - Handles different column types appropriately (strings, numbers, dates, JSON)
/// - Flattens nested JSON structures
/// - Skips NULL values by default
///
/// # Example
///
/// ```ignore
/// use gfs_ai_models::features::extractors::DefaultFeatureExtractor;
/// use gfs_ai_models::features::FeatureExtractor;
///
/// let extractor = DefaultFeatureExtractor::new()
///     .with_separator(" ")
///     .with_column_names(true);
///
/// let text = extractor.extract_node_text(&row, &columns);
/// // Result: "name: John Smith age: 30 city: New York"
/// ```
#[derive(Debug, Clone)]
pub struct DefaultFeatureExtractor {
    /// Whether to include column names in text (e.g., "name: John age: 30")
    pub include_column_names: bool,
    /// Separator between fields
    pub separator: String,
    /// How to represent NULL values (empty string = skip NULL values)
    pub null_representation: String,
    /// Maximum length for a single field (truncate longer values)
    pub max_field_length: usize,
}

impl DefaultFeatureExtractor {
    /// Create a new feature extractor with default settings.
    ///
    /// Defaults:
    /// - include_column_names: false
    /// - separator: " "
    /// - null_representation: "" (skip NULLs)
    /// - max_field_length: 1000
    pub fn new() -> Self {
        Self {
            include_column_names: false,
            separator: " ".to_string(),
            null_representation: String::new(),
            max_field_length: 1000,
        }
    }

    /// Set whether to include column names in the output.
    pub fn with_column_names(mut self, include: bool) -> Self {
        self.include_column_names = include;
        self
    }

    /// Set the separator between fields.
    pub fn with_separator(mut self, separator: impl Into<String>) -> Self {
        self.separator = separator.into();
        self
    }

    /// Set the NULL representation.
    pub fn with_null_representation(mut self, repr: impl Into<String>) -> Self {
        self.null_representation = repr.into();
        self
    }

    /// Set the maximum field length (values longer than this are truncated).
    pub fn with_max_field_length(mut self, length: usize) -> Self {
        self.max_field_length = length;
        self
    }

    /// Format a single column value as text.
    fn format_value(&self, value: &Value, column: &Column) -> String {
        let text = match value {
            // NULL value
            Value::Null => return self.handle_null(column),

            // String types - use as-is
            Value::String(s) => s.clone(),

            // Numbers - format appropriately
            Value::Number(n) => {
                // Check if the column type suggests it's a float/decimal
                let is_float = column.data_type.contains("float")
                    || column.data_type.contains("double")
                    || column.data_type.contains("decimal")
                    || column.data_type.contains("numeric");

                if is_float {
                    if let Some(f) = n.as_f64() {
                        format!("{:.2}", f)
                    } else {
                        n.to_string()
                    }
                } else {
                    n.to_string()
                }
            }

            // Booleans
            Value::Bool(b) => b.to_string(),

            // JSON/JSONB - flatten nested structures
            Value::Object(obj) => self.flatten_json_object(obj),

            // Arrays - concatenate elements
            Value::Array(arr) => self.flatten_json_array(arr),
        };

        // Truncate if too long
        if text.len() > self.max_field_length {
            text.chars().take(self.max_field_length).collect()
        } else {
            text
        }
    }

    /// Flatten a JSON object into text.
    fn flatten_json_object(&self, obj: &serde_json::Map<String, Value>) -> String {
        obj.iter()
            .filter_map(|(k, v)| {
                let val_text = self.flatten_json_value(v);
                if val_text.is_empty() {
                    None
                } else {
                    Some(format!("{}: {}", k, val_text))
                }
            })
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// Flatten a JSON array into text.
    fn flatten_json_array(&self, arr: &[Value]) -> String {
        arr.iter()
            .map(|v| self.flatten_json_value(v))
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// Flatten any JSON value into text.
    fn flatten_json_value(&self, value: &Value) -> String {
        match value {
            Value::Null => String::new(),
            Value::String(s) => s.clone(),
            Value::Number(n) => n.to_string(),
            Value::Bool(b) => b.to_string(),
            Value::Array(arr) => self.flatten_json_array(arr),
            Value::Object(obj) => self.flatten_json_object(obj),
        }
    }
}

impl Default for DefaultFeatureExtractor {
    fn default() -> Self {
        Self::new()
    }
}

impl FeatureExtractor for DefaultFeatureExtractor {
    fn extract_node_text(&self, row: &DatasourceRow, columns: &[Column]) -> String {
        let mut parts = Vec::new();

        for column in columns {
            let value = row.get(&column.name);

            // Skip if value is missing from row
            let value = match value {
                Some(v) => v,
                None => continue,
            };

            // Format the value
            let formatted = self.format_value(value, column);

            // Skip empty values (NULL or empty strings)
            if formatted.is_empty() {
                continue;
            }

            // Include column name if configured
            let text = if self.include_column_names {
                format!("{}: {}", column.name, formatted)
            } else {
                formatted
            };

            parts.push(text);
        }

        parts.join(&self.separator)
    }

    fn handle_null(&self, _column: &Column) -> String {
        self.null_representation.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gfs_domain::model::datasource::Column;
    use serde_json::json;
    use std::collections::HashMap;

    fn make_column(name: &str, data_type: &str) -> Column {
        Column {
            id: "1".to_string(),
            table_id: 1,
            schema: "public".to_string(),
            table: "test".to_string(),
            name: name.to_string(),
            ordinal_position: 1,
            data_type: data_type.to_string(),
            format: "text".to_string(),
            is_identity: false,
            identity_generation: None,
            is_generated: false,
            is_nullable: true,
            is_updatable: true,
            is_unique: false,
            check: None,
            default_value: None,
            enums: vec![],
            comment: None,
        }
    }

    #[test]
    fn test_extract_simple_text() {
        let extractor = DefaultFeatureExtractor::new();
        let mut row = HashMap::new();
        row.insert("name".to_string(), json!("John Smith"));
        row.insert("age".to_string(), json!(30));

        let columns = vec![
            make_column("name", "text"),
            make_column("age", "integer"),
        ];

        let text = extractor.extract_node_text(&row, &columns);
        assert_eq!(text, "John Smith 30");
    }

    #[test]
    fn test_extract_with_column_names() {
        let extractor = DefaultFeatureExtractor::new().with_column_names(true);
        let mut row = HashMap::new();
        row.insert("name".to_string(), json!("Alice"));
        row.insert("city".to_string(), json!("NYC"));

        let columns = vec![
            make_column("name", "text"),
            make_column("city", "text"),
        ];

        let text = extractor.extract_node_text(&row, &columns);
        assert_eq!(text, "name: Alice city: NYC");
    }

    #[test]
    fn test_extract_null_values_skipped() {
        let extractor = DefaultFeatureExtractor::new();
        let mut row = HashMap::new();
        row.insert("name".to_string(), json!("Bob"));
        row.insert("age".to_string(), json!(null));
        row.insert("city".to_string(), json!("LA"));

        let columns = vec![
            make_column("name", "text"),
            make_column("age", "integer"),
            make_column("city", "text"),
        ];

        let text = extractor.extract_node_text(&row, &columns);
        assert_eq!(text, "Bob LA");
    }

    #[test]
    fn test_extract_null_values_with_representation() {
        let extractor = DefaultFeatureExtractor::new().with_null_representation("NULL");
        let mut row = HashMap::new();
        row.insert("name".to_string(), json!("Bob"));
        row.insert("age".to_string(), json!(null));

        let columns = vec![
            make_column("name", "text"),
            make_column("age", "integer"),
        ];

        let text = extractor.extract_node_text(&row, &columns);
        assert_eq!(text, "Bob NULL");
    }

    #[test]
    fn test_extract_custom_separator() {
        let extractor = DefaultFeatureExtractor::new().with_separator(" | ");
        let mut row = HashMap::new();
        row.insert("a".to_string(), json!("foo"));
        row.insert("b".to_string(), json!("bar"));

        let columns = vec![
            make_column("a", "text"),
            make_column("b", "text"),
        ];

        let text = extractor.extract_node_text(&row, &columns);
        assert_eq!(text, "foo | bar");
    }

    #[test]
    fn test_extract_json_flattened() {
        let extractor = DefaultFeatureExtractor::new();
        let mut row = HashMap::new();
        row.insert(
            "metadata".to_string(),
            json!({"tags": ["ml", "ai"], "score": 0.95}),
        );

        let columns = vec![make_column("metadata", "jsonb")];

        let text = extractor.extract_node_text(&row, &columns);
        assert!(text.contains("tags"));
        assert!(text.contains("ml"));
        assert!(text.contains("ai"));
        assert!(text.contains("score"));
    }

    #[test]
    fn test_extract_array_flattened() {
        let extractor = DefaultFeatureExtractor::new();
        let mut row = HashMap::new();
        row.insert("tags".to_string(), json!(["rust", "database", "ml"]));

        let columns = vec![make_column("tags", "array")];

        let text = extractor.extract_node_text(&row, &columns);
        assert_eq!(text, "rust database ml");
    }

    #[test]
    fn test_extract_number_formatting() {
        let extractor = DefaultFeatureExtractor::new();
        let mut row = HashMap::new();
        row.insert("count".to_string(), json!(42));
        row.insert("price".to_string(), json!(19.99));

        let columns = vec![
            make_column("count", "integer"),
            make_column("price", "float"),
        ];

        let text = extractor.extract_node_text(&row, &columns);
        assert_eq!(text, "42 19.99");
    }

    #[test]
    fn test_extract_boolean() {
        let extractor = DefaultFeatureExtractor::new();
        let mut row = HashMap::new();
        row.insert("active".to_string(), json!(true));
        row.insert("deleted".to_string(), json!(false));

        let columns = vec![
            make_column("active", "boolean"),
            make_column("deleted", "boolean"),
        ];

        let text = extractor.extract_node_text(&row, &columns);
        assert_eq!(text, "true false");
    }

    #[test]
    fn test_extract_empty_row_returns_empty_string() {
        let extractor = DefaultFeatureExtractor::new();
        let row = HashMap::new();
        let columns = vec![make_column("name", "text")];

        let text = extractor.extract_node_text(&row, &columns);
        assert_eq!(text, "");
    }

    #[test]
    fn test_max_field_length_truncation() {
        let extractor = DefaultFeatureExtractor::new().with_max_field_length(10);
        let mut row = HashMap::new();
        row.insert("description".to_string(), json!("This is a very long description that should be truncated"));

        let columns = vec![make_column("description", "text")];

        let text = extractor.extract_node_text(&row, &columns);
        assert_eq!(text.len(), 10);
        assert_eq!(text, "This is a ");
    }
}
