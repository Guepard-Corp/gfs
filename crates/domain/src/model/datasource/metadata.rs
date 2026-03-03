//! Top-level datasource metadata. Mirrors `@domain/entities/datasource-meta` DatasourceMetadata (Zod).

use serde::{Deserialize, Serialize};

use super::column_privileges::ColumnPrivileges;
use super::columns::Column;
use super::config::Config;
use super::db_types::DbType;
use super::extensions::DatasourceExtension;
use super::foreign_tables::ForeignTable;
use super::functions::Function;
use super::indexes::Index;
use super::materialized_views::MaterializedView;
use super::policies::Policy;
use super::publications::Publication;
use super::roles::Role;
use super::schema::Schema;
use super::table_privileges::TablePrivileges;
use super::tables::Table;
use super::triggers::Trigger;
use super::views::View;

/// Metadata for a single database within a datasource instance.
///
/// PostgreSQL and MySQL can have multiple databases per instance.
/// This structure groups all schemas, tables, and other objects by database.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DatabaseMetadata {
    /// Database name (e.g., "postgres", "demo", "myapp")
    pub name: String,

    /// Schemas within this database
    pub schemas: Vec<Schema>,

    /// Tables within this database
    pub tables: Vec<Table>,

    /// Columns within this database
    pub columns: Vec<Column>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub views: Option<Vec<View>>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub functions: Option<Vec<Function>>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub indexes: Option<Vec<Index>>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub triggers: Option<Vec<Trigger>>,

    #[serde(rename = "materializedViews", skip_serializing_if = "Option::is_none")]
    pub materialized_views: Option<Vec<MaterializedView>>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub types: Option<Vec<DbType>>,

    #[serde(rename = "foreignTables", skip_serializing_if = "Option::is_none")]
    pub foreign_tables: Option<Vec<ForeignTable>>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub policies: Option<Vec<Policy>>,

    #[serde(rename = "tablePrivileges", skip_serializing_if = "Option::is_none")]
    pub table_privileges: Option<Vec<TablePrivileges>>,

    #[serde(rename = "columnPrivileges", skip_serializing_if = "Option::is_none")]
    pub column_privileges: Option<Vec<ColumnPrivileges>>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub config: Option<Vec<Config>>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub publications: Option<Vec<Publication>>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub roles: Option<Vec<Role>>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub extensions: Option<Vec<DatasourceExtension>>,
}

/// Full catalog metadata returned by a datasource driver.
///
/// Supports both single-database (old format) and multi-database (new format) representations.
/// The multi-database format is used for PostgreSQL/MySQL instances with multiple databases.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DatasourceMetadata {
    pub version: String,
    pub driver: String,

    /// Multi-database format (new): List of databases with their metadata.
    /// When present, this takes precedence over the flat fields below.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub databases: Option<Vec<DatabaseMetadata>>,

    // ===== LEGACY FIELDS (for backward compatibility) =====
    // These fields are deprecated and only used when `databases` is None.
    // When reading old format, these are migrated to a single database in `databases`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub schemas: Option<Vec<Schema>>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub tables: Option<Vec<Table>>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub columns: Option<Vec<Column>>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub views: Option<Vec<View>>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub functions: Option<Vec<Function>>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub indexes: Option<Vec<Index>>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub triggers: Option<Vec<Trigger>>,

    #[serde(rename = "materializedViews", skip_serializing_if = "Option::is_none")]
    pub materialized_views: Option<Vec<MaterializedView>>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub types: Option<Vec<DbType>>,

    #[serde(rename = "foreignTables", skip_serializing_if = "Option::is_none")]
    pub foreign_tables: Option<Vec<ForeignTable>>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub policies: Option<Vec<Policy>>,

    #[serde(rename = "tablePrivileges", skip_serializing_if = "Option::is_none")]
    pub table_privileges: Option<Vec<TablePrivileges>>,

    #[serde(rename = "columnPrivileges", skip_serializing_if = "Option::is_none")]
    pub column_privileges: Option<Vec<ColumnPrivileges>>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub config: Option<Vec<Config>>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub publications: Option<Vec<Publication>>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub roles: Option<Vec<Role>>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub extensions: Option<Vec<DatasourceExtension>>,
}

impl DatasourceMetadata {
    /// Get all databases.
    ///
    /// Returns a reference to the databases vector.
    pub fn get_databases(&self) -> Vec<&DatabaseMetadata> {
        self.databases
            .as_ref()
            .map(|dbs| dbs.iter().collect())
            .unwrap_or_default()
    }

    /// Get a specific database by name.
    pub fn get_database(&self, name: &str) -> Option<&DatabaseMetadata> {
        self.databases.as_ref()?.iter().find(|db| db.name == name)
    }

    /// Get the default/first database.
    ///
    /// Returns the first database in the list, or None if no databases exist.
    pub fn get_default_database(&self) -> Option<&DatabaseMetadata> {
        self.databases.as_ref()?.first()
    }

    /// Get all tables across all databases.
    pub fn get_all_tables(&self) -> Vec<&Table> {
        self.databases
            .as_ref()
            .map(|dbs| dbs.iter().flat_map(|db| db.tables.iter()).collect())
            .unwrap_or_default()
    }

    /// Get all schemas across all databases.
    pub fn get_all_schemas(&self) -> Vec<&Schema> {
        self.databases
            .as_ref()
            .map(|dbs| dbs.iter().flat_map(|db| db.schemas.iter()).collect())
            .unwrap_or_default()
    }

    /// Get all columns across all databases.
    pub fn get_all_columns(&self) -> Vec<&Column> {
        self.databases
            .as_ref()
            .map(|dbs| dbs.iter().flat_map(|db| db.columns.iter()).collect())
            .unwrap_or_default()
    }
}
