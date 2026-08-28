//! Function metadata. Mirrors the datasource metadata schema.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FunctionArgMode {
    In,
    Out,
    Inout,
    Variadic,
    Table,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionArg {
    pub mode: FunctionArgMode,
    pub name: String,
    pub type_id: i64,
    pub has_default: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Function {
    pub id: i64,
    pub schema: String,
    pub name: String,
    pub language: String,
    pub definition: String,
    pub complete_statement: String,
    pub args: Vec<FunctionArg>,
    pub argument_types: String,
    pub identity_argument_types: String,
    pub return_type_id: i64,
    pub return_type: String,
    pub return_type_relation_id: Option<i64>,
    pub is_set_returning_function: bool,
    pub config_params: Option<std::collections::HashMap<String, String>>,
}
