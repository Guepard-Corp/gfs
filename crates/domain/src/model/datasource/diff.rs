//! Schema diff engine for comparing database schemas between commits.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use super::{Column, DatasourceMetadata, Table};

/// Types of diff operations.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub enum DiffOperation {
    Add,
    Drop,
    Modify,
}

/// Types of schema entities.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub enum DiffEntity {
    Table,
    Column,
    PrimaryKey,
    ForeignKey,
    Index,
    View,
    Function,
}

/// A single schema mutation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SchemaMutation {
    pub entity: DiffEntity,
    pub operation: DiffOperation,
    pub target: String,
    pub metadata: HashMap<String, String>,
    pub is_breaking: bool,
}

/// Complete schema diff result.
#[derive(Debug, Serialize, Deserialize)]
pub struct SchemaDiff {
    pub from_commit: String,
    pub to_commit: String,
    pub mutations: Vec<SchemaMutation>,
    pub has_breaking_changes: bool,
}

impl SchemaDiff {
    /// Determine exit code based on changes.
    pub fn exit_code(&self) -> i32 {
        if self.mutations.is_empty() {
            0 // No changes
        } else if self.has_breaking_changes {
            2 // Breaking changes
        } else {
            1 // Safe changes only
        }
    }
}

/// Compute the diff between two schema versions.
pub fn compute_schema_diff(
    old_metadata: &DatasourceMetadata,
    new_metadata: &DatasourceMetadata,
    from_commit: &str,
    to_commit: &str,
) -> SchemaDiff {
    let mut mutations = Vec::new();

    // Compare tables
    diff_tables(old_metadata, new_metadata, &mut mutations);

    // Compare columns
    diff_columns(old_metadata, new_metadata, &mut mutations);

    // Check for breaking changes
    let has_breaking_changes = mutations.iter().any(|m| m.is_breaking);

    SchemaDiff {
        from_commit: from_commit.to_string(),
        to_commit: to_commit.to_string(),
        mutations,
        has_breaking_changes,
    }
}

/// Compare tables between old and new metadata.
fn diff_tables(
    old_metadata: &DatasourceMetadata,
    new_metadata: &DatasourceMetadata,
    mutations: &mut Vec<SchemaMutation>,
) {
    let old_tables: HashMap<String, &Table> = old_metadata
        .tables
        .iter()
        .map(|t| (format!("{}.{}", t.schema, t.name), t))
        .collect();

    let new_tables: HashMap<String, &Table> = new_metadata
        .tables
        .iter()
        .map(|t| (format!("{}.{}", t.schema, t.name), t))
        .collect();

    // Find dropped tables
    for table_name in old_tables.keys() {
        if !new_tables.contains_key(table_name) {
            mutations.push(SchemaMutation {
                entity: DiffEntity::Table,
                operation: DiffOperation::Drop,
                target: table_name.clone(),
                metadata: HashMap::new(),
                is_breaking: true, // Dropping a table is always breaking
            });
        }
    }

    // Find added tables
    for (table_name, table) in &new_tables {
        if !old_tables.contains_key(table_name) {
            let mut metadata = HashMap::new();
            if let Some(comment) = &table.comment {
                metadata.insert("comment".to_string(), comment.clone());
            }

            mutations.push(SchemaMutation {
                entity: DiffEntity::Table,
                operation: DiffOperation::Add,
                target: table_name.clone(),
                metadata,
                is_breaking: false, // Adding a table is safe
            });
        }
    }
}

/// Compare columns between old and new metadata.
fn diff_columns(
    old_metadata: &DatasourceMetadata,
    new_metadata: &DatasourceMetadata,
    mutations: &mut Vec<SchemaMutation>,
) {
    // Build column maps using schema.table.column as key
    let old_columns: HashMap<String, &Column> = old_metadata
        .columns
        .iter()
        .map(|c| (format!("{}.{}.{}", c.schema, c.table, c.name), c))
        .collect();

    let new_columns: HashMap<String, &Column> = new_metadata
        .columns
        .iter()
        .map(|c| (format!("{}.{}.{}", c.schema, c.table, c.name), c))
        .collect();

    // Find dropped columns
    for (column_name, old_col) in &old_columns {
        if !new_columns.contains_key(column_name) {
            let mut metadata = HashMap::new();
            metadata.insert("type".to_string(), old_col.data_type.clone());
            metadata.insert("nullable".to_string(), old_col.is_nullable.to_string());

            mutations.push(SchemaMutation {
                entity: DiffEntity::Column,
                operation: DiffOperation::Drop,
                target: column_name.clone(),
                metadata,
                is_breaking: true, // Dropping a column is breaking
            });
        }
    }

    // Find added columns
    for (column_name, new_col) in &new_columns {
        if !old_columns.contains_key(column_name) {
            let mut metadata = HashMap::new();
            metadata.insert("type".to_string(), new_col.data_type.clone());
            metadata.insert("nullable".to_string(), new_col.is_nullable.to_string());
            if let Some(default) = &new_col.default_value {
                metadata.insert("default".to_string(), default.to_string());
            }

            // Adding a NOT NULL column without a default is breaking
            let is_breaking = !new_col.is_nullable && new_col.default_value.is_none();

            mutations.push(SchemaMutation {
                entity: DiffEntity::Column,
                operation: DiffOperation::Add,
                target: column_name.clone(),
                metadata,
                is_breaking,
            });
        }
    }

    // Find modified columns
    for (column_name, old_col) in &old_columns {
        if let Some(new_col) = new_columns.get(column_name) {
            let mut changes = HashMap::new();
            let mut is_breaking = false;

            // Check type change
            if old_col.data_type != new_col.data_type {
                let type_change = format!("{}->{}", old_col.data_type, new_col.data_type);
                changes.insert("type".to_string(), type_change.clone());

                // Check if it's a narrowing change
                if is_type_narrowing(&old_col.data_type, &new_col.data_type) {
                    is_breaking = true;
                }
            }

            // Check nullable change
            if old_col.is_nullable != new_col.is_nullable {
                let nullable_change = format!("{}->{}", old_col.is_nullable, new_col.is_nullable);
                changes.insert("nullable".to_string(), nullable_change);

                // Adding NOT NULL constraint is breaking
                if old_col.is_nullable && !new_col.is_nullable {
                    is_breaking = true;
                }
            }

            // Check default value change
            if old_col.default_value != new_col.default_value {
                let old_default = old_col
                    .default_value
                    .as_ref()
                    .map(|v| v.to_string())
                    .unwrap_or_else(|| "NULL".to_string());
                let new_default = new_col
                    .default_value
                    .as_ref()
                    .map(|v| v.to_string())
                    .unwrap_or_else(|| "NULL".to_string());
                changes.insert(
                    "default".to_string(),
                    format!("{}->{}", old_default, new_default),
                );
            }

            // If there are changes, add a mutation
            if !changes.is_empty() {
                mutations.push(SchemaMutation {
                    entity: DiffEntity::Column,
                    operation: DiffOperation::Modify,
                    target: column_name.clone(),
                    metadata: changes,
                    is_breaking,
                });
            }
        }
    }
}

/// Check if a type change is narrowing (breaking).
fn is_type_narrowing(old_type: &str, new_type: &str) -> bool {
    // Parse varchar lengths
    if let (Some(old_len), Some(new_len)) = (
        extract_varchar_length(old_type),
        extract_varchar_length(new_type),
    ) {
        return new_len < old_len; // varchar(255) -> varchar(100) is narrowing
    }

    // Common narrowing patterns
    match (old_type, new_type) {
        ("bigint", "int") | ("bigint", "smallint") | ("bigint", "integer") => true,
        ("int", "smallint") | ("integer", "smallint") => true,
        ("double precision", "real") => true,
        ("text", s) if s.starts_with("varchar") => true,
        ("numeric", "int") | ("numeric", "integer") | ("numeric", "bigint") => true,
        _ => false,
    }
}

/// Extract varchar length from type string like "varchar(255)".
fn extract_varchar_length(type_str: &str) -> Option<usize> {
    if !type_str.starts_with("varchar") && !type_str.starts_with("character varying") {
        return None;
    }

    let start = type_str.find('(')?;
    let end = type_str.find(')')?;
    let len_str = &type_str[start + 1..end];
    len_str.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_diff_exit_code_no_changes() {
        let diff = SchemaDiff {
            from_commit: "abc123".to_string(),
            to_commit: "def456".to_string(),
            mutations: vec![],
            has_breaking_changes: false,
        };
        assert_eq!(diff.exit_code(), 0);
    }

    #[test]
    fn test_diff_exit_code_safe_changes() {
        let diff = SchemaDiff {
            from_commit: "abc123".to_string(),
            to_commit: "def456".to_string(),
            mutations: vec![SchemaMutation {
                entity: DiffEntity::Column,
                operation: DiffOperation::Add,
                target: "users.email".to_string(),
                metadata: HashMap::new(),
                is_breaking: false,
            }],
            has_breaking_changes: false,
        };
        assert_eq!(diff.exit_code(), 1);
    }

    #[test]
    fn test_diff_exit_code_breaking_changes() {
        let diff = SchemaDiff {
            from_commit: "abc123".to_string(),
            to_commit: "def456".to_string(),
            mutations: vec![SchemaMutation {
                entity: DiffEntity::Table,
                operation: DiffOperation::Drop,
                target: "orders".to_string(),
                metadata: HashMap::new(),
                is_breaking: true,
            }],
            has_breaking_changes: true,
        };
        assert_eq!(diff.exit_code(), 2);
    }

    #[test]
    fn test_table_dropped_is_breaking() {
        let mutation = SchemaMutation {
            entity: DiffEntity::Table,
            operation: DiffOperation::Drop,
            target: "orders".to_string(),
            metadata: HashMap::new(),
            is_breaking: true,
        };
        assert!(mutation.is_breaking);
    }

    #[test]
    fn test_column_added_nullable_is_safe() {
        let mut metadata = HashMap::new();
        metadata.insert("nullable".to_string(), "true".to_string());

        let mutation = SchemaMutation {
            entity: DiffEntity::Column,
            operation: DiffOperation::Add,
            target: "users.verified_at".to_string(),
            metadata,
            is_breaking: false,
        };
        assert!(!mutation.is_breaking);
    }

    #[test]
    fn test_type_narrowing_varchar() {
        assert!(is_type_narrowing("varchar(255)", "varchar(100)"));
        assert!(!is_type_narrowing("varchar(100)", "varchar(255)"));
    }

    #[test]
    fn test_type_narrowing_integers() {
        assert!(is_type_narrowing("bigint", "int"));
        assert!(is_type_narrowing("int", "smallint"));
        assert!(!is_type_narrowing("int", "bigint"));
    }

    #[test]
    fn test_extract_varchar_length() {
        assert_eq!(extract_varchar_length("varchar(255)"), Some(255));
        assert_eq!(extract_varchar_length("varchar(100)"), Some(100));
        assert_eq!(extract_varchar_length("character varying(50)"), Some(50));
        assert_eq!(extract_varchar_length("text"), None);
        assert_eq!(extract_varchar_length("int"), None);
    }
}
