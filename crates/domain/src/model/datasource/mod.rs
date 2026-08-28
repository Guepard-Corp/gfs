//! Datasource metadata and result set types.
//!
//! Plain Rust mirror of the datasource metadata schema.
//! JSON (de)serialization follows the same shape so metadata/resultset can be
//! exchanged or validated on the Rust side when needed.

mod column_privileges;
mod columns;
mod config;
mod db_types;
pub mod diff;
pub mod diff_formatter;
mod extensions;
mod foreign_tables;
mod functions;
mod indexes;
mod materialized_views;
mod metadata;
mod policies;
mod publications;
mod resultset;
mod roles;
mod schema;
mod table_privileges;
mod tables;
mod triggers;
mod version;
mod views;

pub use column_privileges::{ColumnPrivilegeGrant, ColumnPrivilegeType, ColumnPrivileges};
pub use columns::Column;
pub use config::Config;
pub use db_types::{DbType, TypeAttribute};
pub use extensions::DatasourceExtension;
pub use foreign_tables::ForeignTable;
pub use functions::{Function, FunctionArg, FunctionArgMode};
pub use indexes::{Index, IndexAttribute};
pub use materialized_views::MaterializedView;
pub use metadata::DatasourceMetadata;
pub use policies::{Policy, PolicyAction, PolicyCommand};
pub use publications::{Publication, PublicationTable};
pub use resultset::{
    ColumnHeader, ColumnType, DatasourceResultSet, DatasourceResultStat, DatasourceRow,
};
pub use roles::Role;
pub use schema::Schema;
pub use table_privileges::{
    TablePrivilegeGrant, TablePrivilegeType, TablePrivileges, TablePrivilegesKind,
};
pub use tables::{Table, TablePrimaryKey, TableRelationship};
pub use triggers::Trigger;
pub use version::DatasourceVersion;
pub use views::View;
