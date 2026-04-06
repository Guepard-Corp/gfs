//! `gfs providers` — list database providers and their supported versions (RFC 006).

use std::sync::Arc;

use anyhow::{Context, Result};
use gfs_domain::ports::database_provider::{
    DatabaseProviderRegistry, InMemoryDatabaseProviderRegistry, SupportedFeature,
};

use crate::output::{
    bold, cyan, tbl_rule, TBL_BL, TBL_BR, TBL_T_DOWN, TBL_T_LEFT, TBL_T_RIGHT, TBL_T_UP,
    TBL_TL, TBL_TR, TBL_V,
};

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

pub fn run(provider_name: Option<String>) -> Result<()> {
    let registry = Arc::new(InMemoryDatabaseProviderRegistry::new());
    gfs_compute_docker::containers::register_all(registry.as_ref())
        .context("failed to register database providers")?;

    match provider_name {
        Some(name) => print_provider_detail(registry.as_ref(), &name)?,
        None => print_all_providers(registry.as_ref())?,
    }
    Ok(())
}

fn print_all_providers(registry: &impl DatabaseProviderRegistry) -> Result<()> {
    let names = registry.list();
    if names.is_empty() {
        println!("  (no providers registered)");
        return Ok(());
    }

    let rows: Vec<_> = names
        .into_iter()
        .filter_map(|name| {
            let provider = registry.get(&name)?;
            let versions = provider.supported_versions().join(", ");
            let features = provider
                .supported_features()
                .iter()
                .map(|f| f.id.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            Some((name, versions, features))
        })
        .collect();

    print_providers_table(&rows);
    Ok(())
}

fn print_provider_detail(registry: &impl DatabaseProviderRegistry, name: &str) -> Result<()> {
    let provider = registry
        .get(name)
        .ok_or_else(|| anyhow::anyhow!("unknown provider: '{}'", name))?;

    let versions = provider.supported_versions();
    let features = provider.supported_features();

    println!("  {} {}", bold("Provider:"), cyan(name));
    println!();
    println!("  Supported versions: {}", versions.join(", "));
    println!();
    print_features_table(&features);
    println!();
    println!("  Images are pulled from Docker Hub by default.");
    Ok(())
}

// ---------------------------------------------------------------------------
// Output — Unicode box tables
// ---------------------------------------------------------------------------

const COL_PROVIDER: usize = 20;
const COL_VERSION: usize = 30;
const COL_FEATURES: usize = 30;

fn print_providers_table(rows: &[(String, String, String)]) {
    let cols = [COL_PROVIDER, COL_VERSION, COL_FEATURES];

    // Top border: ┌──────┬──────┬──────┐
    println!("{}", tbl_rule(&cols, TBL_TL, TBL_T_DOWN, TBL_TR));

    // Header row
    let h_provider = format!("{:<w$}", "database_provider", w = COL_PROVIDER);
    let h_version = format!("{:<w$}", "version", w = COL_VERSION);
    let h_features = format!("{:<w$}", "features", w = COL_FEATURES);
    println!(
        "  {} {} {} {} {} {} {}",
        TBL_V,
        bold(&h_provider),
        TBL_V,
        bold(&h_version),
        TBL_V,
        bold(&h_features),
        TBL_V
    );

    // Separator: ├──────┼──────┼──────┤
    println!(
        "{}",
        tbl_rule(&cols, TBL_T_RIGHT, "┼", TBL_T_LEFT)
    );

    // Data rows
    for (name, versions, features) in rows {
        let p = format!("{:<w$}", name, w = COL_PROVIDER);
        let v = format!("{:<w$}", truncate(versions, COL_VERSION), w = COL_VERSION);
        let f = format!("{:<w$}", truncate(features, COL_FEATURES), w = COL_FEATURES);
        println!(
            "  {} {} {} {} {} {} {}",
            TBL_V,
            cyan(&p),
            TBL_V,
            v,
            TBL_V,
            f,
            TBL_V
        );
    }

    // Bottom border: └──────┴──────┴──────┘
    println!("{}", tbl_rule(&cols, TBL_BL, TBL_T_UP, TBL_BR));

    println!();
    println!("  Images are pulled from Docker Hub by default.");
}

const COL_FEATURE: usize = 25;
const COL_DESC: usize = 45;

fn print_features_table(features: &[SupportedFeature]) {
    let cols = [COL_FEATURE, COL_DESC];

    println!("  {}", bold("Features"));

    // Top border
    println!("{}", tbl_rule(&cols, TBL_TL, TBL_T_DOWN, TBL_TR));

    // Header
    let h_feat = format!("{:<w$}", "feature", w = COL_FEATURE);
    let h_desc = format!("{:<w$}", "description", w = COL_DESC);
    println!(
        "  {} {} {} {} {}",
        TBL_V,
        bold(&h_feat),
        TBL_V,
        bold(&h_desc),
        TBL_V
    );

    // Separator
    println!(
        "{}",
        tbl_rule(&cols, TBL_T_RIGHT, "┼", TBL_T_LEFT)
    );

    // Rows
    for f in features {
        let feat = format!("{:<w$}", f.id, w = COL_FEATURE);
        let desc = format!(
            "{:<w$}",
            truncate(&f.description, COL_DESC),
            w = COL_DESC
        );
        println!(
            "  {} {} {} {} {}",
            TBL_V, feat, TBL_V, desc, TBL_V
        );
    }

    // Bottom
    println!("{}", tbl_rule(&cols, TBL_BL, TBL_T_UP, TBL_BR));
}

fn truncate(s: impl AsRef<str>, max_len: usize) -> String {
    let s = s.as_ref();
    if s.len() <= max_len {
        s.to_string()
    } else {
        format!("{}…", &s[..max_len.saturating_sub(1)])
    }
}
