//! Formatters for schema diff output (agentic, pretty, and JSON).

use std::collections::HashMap;
use std::io::IsTerminal;

use serde::Serialize;

use super::diff::{DiffEntity, DiffOperation, SchemaDiff, SchemaMutation};

/// Agentic formatter (default) - machine-readable, line-oriented format.
pub struct AgenticFormatter;

impl AgenticFormatter {
    /// Format a schema diff in agentic format.
    pub fn format(diff: &SchemaDiff) -> String {
        let mut lines = Vec::new();

        // Header line
        lines.push(format!(
            "GFS_DIFF v1 from={} to={} breaking={}",
            &diff.from_commit[..7.min(diff.from_commit.len())],
            &diff.to_commit[..7.min(diff.to_commit.len())],
            diff.has_breaking_changes
        ));

        // Sort mutations: entity type, then operation, then target
        let mut sorted = diff.mutations.clone();
        sorted.sort_by(|a, b| {
            entity_order(&a.entity)
                .cmp(&entity_order(&b.entity))
                .then_with(|| operation_order(&a.operation).cmp(&operation_order(&b.operation)))
                .then_with(|| a.target.cmp(&b.target))
        });

        // Format each mutation
        for mutation in sorted {
            lines.push(Self::format_mutation(&mutation));
        }

        lines.join("\n")
    }

    fn format_mutation(m: &SchemaMutation) -> String {
        let entity = format!("{:?}", m.entity).to_uppercase();
        let operation = format!("{:?}", m.operation).to_uppercase();
        let mut parts = vec![entity, operation, m.target.clone()];

        // Add sorted key=value metadata
        let mut metadata_vec: Vec<_> = m.metadata.iter().collect();
        metadata_vec.sort_by_key(|(k, _)| *k);

        for (k, v) in metadata_vec {
            parts.push(format!("{}={}", k, v));
        }

        if m.is_breaking {
            parts.push("breaking=true".to_string());
        }

        parts.join(" ")
    }
}

/// Pretty formatter - human-readable visual format with colors.
pub struct PrettyFormatter {
    use_color: bool,
}

impl PrettyFormatter {
    /// Create a new pretty formatter.
    pub fn new(use_color: bool) -> Self {
        // Auto-detection: disable if NO_COLOR env var or stdout is not a TTY
        let use_color =
            use_color && std::env::var("NO_COLOR").is_err() && std::io::stdout().is_terminal();

        Self { use_color }
    }

    /// Format a schema diff in pretty format.
    pub fn format(&self, diff: &SchemaDiff) -> String {
        let mut output = String::new();

        // Header
        output.push_str(&format!(
            "Schema diff  main@{} → main@{}\n",
            &diff.from_commit[..7.min(diff.from_commit.len())],
            &diff.to_commit[..7.min(diff.to_commit.len())]
        ));
        output.push_str("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━\n");

        if diff.mutations.is_empty() {
            output.push_str("  No changes\n");
        } else {
            // Group mutations by table
            let grouped = self.group_by_table(&diff.mutations);

            for (table_name, mutations) in grouped {
                output.push_str(&self.format_table_changes(&table_name, &mutations));
            }
        }

        // Footer
        output.push_str("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━\n");
        output.push_str(&self.format_summary(diff));

        output
    }

    fn group_by_table<'a>(
        &self,
        mutations: &'a [SchemaMutation],
    ) -> Vec<(String, Vec<&'a SchemaMutation>)> {
        let mut groups: HashMap<String, Vec<&SchemaMutation>> = HashMap::new();

        for mutation in mutations {
            let table_name = match &mutation.entity {
                DiffEntity::Table => mutation.target.clone(),
                DiffEntity::Column => {
                    // Extract table name from "schema.table.column"
                    let parts: Vec<&str> = mutation.target.rsplitn(2, '.').collect();
                    if parts.len() == 2 {
                        parts[1].to_string()
                    } else {
                        mutation.target.clone()
                    }
                }
                _ => mutation.target.clone(),
            };

            groups.entry(table_name).or_default().push(mutation);
        }

        let mut result: Vec<_> = groups.into_iter().collect();
        result.sort_by(|(a, _), (b, _)| a.cmp(b));
        result
    }

    fn format_table_changes(&self, table_name: &str, mutations: &[&SchemaMutation]) -> String {
        let mut output = String::new();

        // Separate table-level and column-level mutations
        let table_mutations: Vec<_> = mutations
            .iter()
            .filter(|m| m.entity == DiffEntity::Table)
            .collect();
        let other_mutations: Vec<_> = mutations
            .iter()
            .filter(|m| m.entity != DiffEntity::Table)
            .collect();

        // Format table-level changes
        for mutation in &table_mutations {
            output.push_str(&self.format_mutation_line(mutation, 0));
        }

        // Format column and other changes
        if !other_mutations.is_empty() {
            if table_mutations.is_empty() {
                // No table-level change, show table name
                output.push_str(&format!(
                    "  {}    {}\n",
                    self.colorize("MODIFIED", Color::Bold),
                    self.colorize(table_name, Color::Bold)
                ));
            }

            for mutation in other_mutations {
                output.push_str(&self.format_mutation_line(mutation, 1));
            }
        }

        output
    }

    fn format_mutation_line(&self, mutation: &SchemaMutation, indent_level: usize) -> String {
        let indent = if indent_level == 0 {
            "  ".to_string()
        } else {
            "  │  ".to_string()
        };

        let (symbol, color) = match mutation.operation {
            DiffOperation::Add => ("+", Color::Green),
            DiffOperation::Drop => ("✕", Color::Red),
            DiffOperation::Modify => ("~", Color::Yellow),
        };

        let symbol_colored = self.colorize(symbol, color);

        // Format entity type
        let entity_str = match mutation.entity {
            DiffEntity::Table => "table",
            DiffEntity::Column => "column",
            DiffEntity::PrimaryKey => "pk",
            DiffEntity::ForeignKey => "fk",
            DiffEntity::Index => "index",
            DiffEntity::View => "view",
            DiffEntity::Function => "function",
        };

        // Extract just the column/entity name (not the full path)
        let target = if mutation.entity == DiffEntity::Column {
            mutation
                .target
                .split('.')
                .next_back()
                .unwrap_or(&mutation.target)
        } else {
            &mutation.target
        };

        // Format metadata
        let mut metadata_parts = Vec::new();
        if let Some(type_info) = mutation.metadata.get("type") {
            metadata_parts.push(self.colorize(type_info, Color::Dim));
        }
        if let Some(nullable) = mutation.metadata.get("nullable")
            && nullable == "true"
        {
            metadata_parts.push(self.colorize("nullable", Color::Dim));
        }

        let metadata_str = if metadata_parts.is_empty() {
            String::new()
        } else {
            format!("    {}", metadata_parts.join(" "))
        };

        let breaking_tag = if mutation.is_breaking {
            format!("  {}", self.colorize("[BREAKING]", Color::Red))
        } else {
            String::new()
        };

        format!(
            "{}{} {:9} {:20}{}{}",
            indent, symbol_colored, entity_str, target, metadata_str, breaking_tag
        )
        .trim_end()
        .to_string()
            + "\n"
    }

    fn format_summary(&self, diff: &SchemaDiff) -> String {
        let mut output = String::new();

        // Count changes by type
        let added = diff
            .mutations
            .iter()
            .filter(|m| m.operation == DiffOperation::Add)
            .count();
        let dropped = diff
            .mutations
            .iter()
            .filter(|m| m.operation == DiffOperation::Drop)
            .count();
        let modified = diff
            .mutations
            .iter()
            .filter(|m| m.operation == DiffOperation::Modify)
            .count();

        let mut summary_parts = Vec::new();
        if added > 0 {
            summary_parts.push(format!("{} added", added));
        }
        if modified > 0 {
            summary_parts.push(format!("{} modified", modified));
        }
        if dropped > 0 {
            summary_parts.push(format!("{} dropped", dropped));
        }

        let summary = if summary_parts.is_empty() {
            "No changes".to_string()
        } else {
            summary_parts.join(" · ")
        };

        output.push_str(&format!(
            "  {}   {}\n",
            self.colorize("Summary", Color::Bold),
            summary
        ));

        // Risk line
        if diff.has_breaking_changes {
            output.push_str(&format!(
                "  {}      {} {}\n",
                self.colorize("Risk", Color::Bold),
                self.colorize("⚠", Color::Yellow),
                self.colorize("BREAKING CHANGES", Color::Red)
            ));
        } else if !diff.mutations.is_empty() {
            output.push_str(&format!(
                "  {}      {} {}\n",
                self.colorize("Risk", Color::Bold),
                self.colorize("✓", Color::Green),
                self.colorize("Safe changes", Color::Green)
            ));
        }

        output
    }

    fn colorize(&self, text: &str, color: Color) -> String {
        if !self.use_color {
            return text.to_string();
        }

        format!("\x1b[{}m{}\x1b[0m", color.code(), text)
    }
}

/// ANSI color codes.
enum Color {
    Red,
    Green,
    Yellow,
    Dim,
    Bold,
}

impl Color {
    fn code(&self) -> &str {
        match self {
            Color::Red => "31",
            Color::Green => "32",
            Color::Yellow => "33",
            Color::Dim => "2",
            Color::Bold => "1",
        }
    }
}

/// Determine sort order for entities.
fn entity_order(entity: &DiffEntity) -> u8 {
    match entity {
        DiffEntity::Table => 0,
        DiffEntity::Column => 1,
        DiffEntity::PrimaryKey => 2,
        DiffEntity::ForeignKey => 3,
        DiffEntity::Index => 4,
        DiffEntity::View => 5,
        DiffEntity::Function => 6,
    }
}

/// Determine sort order for operations.
fn operation_order(operation: &DiffOperation) -> u8 {
    match operation {
        DiffOperation::Drop => 0,
        DiffOperation::Modify => 1,
        DiffOperation::Add => 2,
    }
}

// ---------------------------------------------------------------------------
// JSON Formatter
// ---------------------------------------------------------------------------

/// JSON formatter - structured JSON output for APIs and tools.
pub struct JsonFormatter;

impl JsonFormatter {
    /// Format a schema diff as JSON.
    pub fn format(diff: &SchemaDiff) -> Result<String, serde_json::Error> {
        let output = JsonSchemaDiff {
            version: "1".to_string(),
            from_commit: diff.from_commit.clone(),
            to_commit: diff.to_commit.clone(),
            has_breaking_changes: diff.has_breaking_changes,
            exit_code: diff.exit_code(),
            mutations: &diff.mutations,
            summary: SchemaDiffSummary::from_diff(diff),
        };
        serde_json::to_string_pretty(&output)
    }
}

/// JSON wrapper for schema diff output.
#[derive(Serialize)]
struct JsonSchemaDiff<'a> {
    version: String,
    from_commit: String,
    to_commit: String,
    has_breaking_changes: bool,
    exit_code: i32,
    mutations: &'a [SchemaMutation],
    summary: SchemaDiffSummary,
}

/// Summary statistics for schema diff.
#[derive(Serialize)]
struct SchemaDiffSummary {
    total: usize,
    by_operation: HashMap<String, usize>,
    by_entity: HashMap<String, usize>,
}

impl SchemaDiffSummary {
    /// Compute summary statistics from a diff.
    fn from_diff(diff: &SchemaDiff) -> Self {
        let mut by_operation: HashMap<String, usize> = HashMap::new();
        let mut by_entity: HashMap<String, usize> = HashMap::new();

        for mutation in &diff.mutations {
            let op_key = format!("{:?}", mutation.operation);
            *by_operation.entry(op_key).or_insert(0) += 1;

            let entity_key = format!("{:?}", mutation.entity);
            *by_entity.entry(entity_key).or_insert(0) += 1;
        }

        Self {
            total: diff.mutations.len(),
            by_operation,
            by_entity,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::datasource::diff::SchemaMutation;

    fn create_test_diff() -> SchemaDiff {
        SchemaDiff {
            from_commit: "a3f1c2d".to_string(),
            to_commit: "b7d4e9a".to_string(),
            mutations: vec![],
            has_breaking_changes: false,
        }
    }

    #[test]
    fn test_agentic_format_header() {
        let diff = create_test_diff();
        let output = AgenticFormatter::format(&diff);
        assert!(output.starts_with("GFS_DIFF v1"));
        assert!(output.contains("from=a3f1c2d"));
        assert!(output.contains("to=b7d4e9a"));
        assert!(output.contains("breaking=false"));
    }

    #[test]
    fn test_agentic_format_table_drop() {
        let mutation = SchemaMutation {
            entity: DiffEntity::Table,
            operation: DiffOperation::Drop,
            target: "orders".to_string(),
            metadata: HashMap::new(),
            is_breaking: true,
        };

        let output = AgenticFormatter::format_mutation(&mutation);
        assert_eq!(output, "TABLE DROP orders breaking=true");
    }

    #[test]
    fn test_agentic_format_column_add() {
        let mut metadata = HashMap::new();
        metadata.insert("type".to_string(), "timestamp".to_string());
        metadata.insert("nullable".to_string(), "true".to_string());

        let mutation = SchemaMutation {
            entity: DiffEntity::Column,
            operation: DiffOperation::Add,
            target: "users.verified_at".to_string(),
            metadata,
            is_breaking: false,
        };

        let output = AgenticFormatter::format_mutation(&mutation);
        assert!(output.starts_with("COLUMN ADD users.verified_at"));
        assert!(output.contains("nullable=true"));
        assert!(output.contains("type=timestamp"));
    }

    #[test]
    fn test_pretty_format_no_color_when_no_color_env() {
        unsafe {
            std::env::set_var("NO_COLOR", "1");
        }
        let formatter = PrettyFormatter::new(true);
        assert!(!formatter.use_color);
        unsafe {
            std::env::remove_var("NO_COLOR");
        }
    }

    #[test]
    fn test_pretty_format_symbols() {
        let mut diff = create_test_diff();

        // Add both table-level and column-level mutations to test tree connectors
        let mut metadata = HashMap::new();
        metadata.insert("type".to_string(), "varchar(255)".to_string());

        diff.mutations = vec![
            SchemaMutation {
                entity: DiffEntity::Table,
                operation: DiffOperation::Add,
                target: "public.users".to_string(),
                metadata: HashMap::new(),
                is_breaking: false,
            },
            SchemaMutation {
                entity: DiffEntity::Column,
                operation: DiffOperation::Add,
                target: "public.users.email".to_string(),
                metadata: metadata.clone(),
                is_breaking: false,
            },
            SchemaMutation {
                entity: DiffEntity::Table,
                operation: DiffOperation::Drop,
                target: "public.orders".to_string(),
                metadata: HashMap::new(),
                is_breaking: true,
            },
        ];
        diff.has_breaking_changes = true;

        let formatter = PrettyFormatter::new(false);
        let output = formatter.format(&diff);

        assert!(output.contains("✕")); // DROP symbol
        assert!(output.contains("+")); // ADD symbol
        assert!(output.contains("│")); // Tree connector for column under table
    }

    #[test]
    fn test_pretty_format_summary() {
        let mut diff = create_test_diff();
        diff.mutations = vec![SchemaMutation {
            entity: DiffEntity::Column,
            operation: DiffOperation::Add,
            target: "users.email".to_string(),
            metadata: HashMap::new(),
            is_breaking: false,
        }];

        let formatter = PrettyFormatter::new(false);
        let output = formatter.format(&diff);

        assert!(output.contains("Summary"));
        assert!(output.contains("Risk"));
        assert!(output.contains("Safe changes"));
    }

    #[test]
    fn test_entity_order() {
        assert!(entity_order(&DiffEntity::Table) < entity_order(&DiffEntity::Column));
        assert!(entity_order(&DiffEntity::Column) < entity_order(&DiffEntity::Index));
    }

    #[test]
    fn test_operation_order() {
        assert!(operation_order(&DiffOperation::Drop) < operation_order(&DiffOperation::Modify));
        assert!(operation_order(&DiffOperation::Modify) < operation_order(&DiffOperation::Add));
    }

    #[test]
    fn test_json_format_no_changes() {
        let diff = create_test_diff();
        let json = JsonFormatter::format(&diff).expect("JSON serialization failed");
        let parsed: serde_json::Value = serde_json::from_str(&json).expect("JSON parsing failed");

        assert_eq!(parsed["version"], "1");
        assert_eq!(parsed["from_commit"], "a3f1c2d");
        assert_eq!(parsed["to_commit"], "b7d4e9a");
        assert_eq!(parsed["has_breaking_changes"], false);
        assert_eq!(parsed["exit_code"], 0);
        assert_eq!(parsed["mutations"].as_array().unwrap().len(), 0);
        assert_eq!(parsed["summary"]["total"], 0);
    }

    #[test]
    fn test_json_format_with_mutations() {
        let mut diff = create_test_diff();

        let mut metadata = HashMap::new();
        metadata.insert("type".to_string(), "varchar(255)".to_string());
        metadata.insert("nullable".to_string(), "true".to_string());

        diff.mutations = vec![
            SchemaMutation {
                entity: DiffEntity::Table,
                operation: DiffOperation::Add,
                target: "public.orders".to_string(),
                metadata: HashMap::new(),
                is_breaking: false,
            },
            SchemaMutation {
                entity: DiffEntity::Column,
                operation: DiffOperation::Drop,
                target: "public.users.legacy_id".to_string(),
                metadata: metadata.clone(),
                is_breaking: true,
            },
        ];
        diff.has_breaking_changes = true;

        let json = JsonFormatter::format(&diff).expect("JSON serialization failed");
        let parsed: serde_json::Value = serde_json::from_str(&json).expect("JSON parsing failed");

        assert_eq!(parsed["version"], "1");
        assert_eq!(parsed["has_breaking_changes"], true);
        assert_eq!(parsed["exit_code"], 2); // Breaking changes
        assert_eq!(parsed["mutations"].as_array().unwrap().len(), 2);

        // Check summary
        assert_eq!(parsed["summary"]["total"], 2);
        assert_eq!(parsed["summary"]["by_operation"]["Add"], 1);
        assert_eq!(parsed["summary"]["by_operation"]["Drop"], 1);
        assert_eq!(parsed["summary"]["by_entity"]["Table"], 1);
        assert_eq!(parsed["summary"]["by_entity"]["Column"], 1);

        // Check mutation details
        let mutations = parsed["mutations"].as_array().unwrap();
        assert_eq!(mutations[0]["entity"], "Table");
        assert_eq!(mutations[0]["operation"], "Add");
        assert_eq!(mutations[0]["target"], "public.orders");
        assert_eq!(mutations[0]["is_breaking"], false);

        assert_eq!(mutations[1]["entity"], "Column");
        assert_eq!(mutations[1]["operation"], "Drop");
        assert_eq!(mutations[1]["target"], "public.users.legacy_id");
        assert_eq!(mutations[1]["is_breaking"], true);
        assert_eq!(mutations[1]["metadata"]["type"], "varchar(255)");
        assert_eq!(mutations[1]["metadata"]["nullable"], "true");
    }

    #[test]
    fn test_json_format_safe_changes() {
        let mut diff = create_test_diff();
        diff.mutations = vec![SchemaMutation {
            entity: DiffEntity::Column,
            operation: DiffOperation::Add,
            target: "users.email".to_string(),
            metadata: HashMap::new(),
            is_breaking: false,
        }];

        let json = JsonFormatter::format(&diff).expect("JSON serialization failed");
        let parsed: serde_json::Value = serde_json::from_str(&json).expect("JSON parsing failed");

        assert_eq!(parsed["exit_code"], 1); // Safe changes
        assert_eq!(parsed["has_breaking_changes"], false);
    }

    #[test]
    fn test_json_roundtrip() {
        let mut diff = create_test_diff();
        let mut metadata = HashMap::new();
        metadata.insert("type".to_string(), "int".to_string());

        diff.mutations = vec![SchemaMutation {
            entity: DiffEntity::Column,
            operation: DiffOperation::Modify,
            target: "users.age".to_string(),
            metadata,
            is_breaking: false,
        }];

        // Serialize to JSON
        let json = JsonFormatter::format(&diff).expect("JSON serialization failed");

        // Parse back
        let parsed: serde_json::Value = serde_json::from_str(&json).expect("JSON parsing failed");

        // Verify all fields are present
        assert!(parsed.get("version").is_some());
        assert!(parsed.get("from_commit").is_some());
        assert!(parsed.get("to_commit").is_some());
        assert!(parsed.get("has_breaking_changes").is_some());
        assert!(parsed.get("exit_code").is_some());
        assert!(parsed.get("mutations").is_some());
        assert!(parsed.get("summary").is_some());

        // Verify summary structure
        let summary = &parsed["summary"];
        assert!(summary.get("total").is_some());
        assert!(summary.get("by_operation").is_some());
        assert!(summary.get("by_entity").is_some());
    }
}
