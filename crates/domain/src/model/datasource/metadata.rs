//! Top-level datasource metadata. Mirrors the datasource metadata schema.

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

/// Full catalog metadata returned by a datasource driver.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DatasourceMetadata {
    pub version: String,
    pub driver: String,
    pub schemas: Vec<Schema>,
    pub tables: Vec<Table>,
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
