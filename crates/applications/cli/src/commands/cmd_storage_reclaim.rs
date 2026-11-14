//! `gfs storage reclaim`: delete the OpenEBS ZFS volumes the driver was asked
//! to delete and never did, so the data of databases destroyed earlier leaves
//! the node. A destroy finishes this for its own volumes; this command covers
//! volumes stranded by a destroy that ran before it could.

use std::io::{IsTerminal, Write};
use std::time::Duration;

use anyhow::Result;
use gfs_storage_kubernetes::{KubernetesStorage, Verdict};
use serde_json::json;

fn verdict_label(v: &Verdict) -> (&'static str, &'static str) {
    match v {
        Verdict::Reclaim => ("reclaim", ""),
        Verdict::Wait(why) => ("blocked", why),
        Verdict::Leave(why) => ("keep", why),
        Verdict::Gone => ("gone", ""),
    }
}

fn mib(bytes: Option<u64>) -> String {
    bytes.map_or_else(
        || "?".to_string(),
        |b| format!("{:.1} MiB", b as f64 / 1_048_576.0),
    )
}

/// Ask on the terminal whether to delete; anything but y/yes is a no.
fn confirm(question: &str) -> bool {
    eprint!("{question} [y/N] ");
    let _ = std::io::stderr().flush();
    let mut answer = String::new();
    let _ = std::io::stdin().read_line(&mut answer);
    matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}

pub async fn run(yes: bool, wait_secs: u64, only: Vec<String>, json_output: bool) -> Result<()> {
    let storage = KubernetesStorage::new(None).await?;
    let mut marked = storage.marked_openebs_volumes().await?;
    if !only.is_empty() {
        // A named volume that is not marked is not a candidate: nothing asked
        // for it to be deleted, so naming it does not make it one.
        marked.retain(|v| only.contains(v));
    }
    let before = storage.assess_openebs_volumes(&marked).await?;

    // At a terminal the listing ends in a question; in a script or a pipe,
    // or with --json, it stays a listing and only --yes deletes.
    let interactive =
        !json_output && std::io::stdin().is_terminal() && std::io::stderr().is_terminal();

    if !yes {
        if json_output {
            let rows: Vec<_> = before
                .iter()
                .map(|a| {
                    let (state, why) = verdict_label(&a.verdict);
                    json!({"volume": a.volume, "state": state, "reason": why, "used_bytes": a.used_bytes})
                })
                .collect();
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({"dry_run": true, "volumes": rows}))?
            );
            return Ok(());
        }
        if before.is_empty() {
            println!("No OpenEBS ZFS volume is waiting to be deleted.");
            return Ok(());
        }
        for a in &before {
            let (state, why) = verdict_label(&a.verdict);
            println!(
                "{state:<8} {:<42} {:>10}  {why}",
                a.volume,
                mib(a.used_bytes)
            );
        }
        let n = before
            .iter()
            .filter(|a| a.verdict == Verdict::Reclaim)
            .count();
        let held = before
            .iter()
            .filter(|a| matches!(a.verdict, Verdict::Wait(_)))
            .count();
        println!("\n{n} volume(s) can be reclaimed now.");
        let question = if held == 0 {
            format!("Delete these {n} volume(s) and their data?")
        } else {
            format!(
                "Delete these {n} volume(s) and their data, and wait up to {wait_secs} s for the {held} blocked one(s)?"
            )
        };
        if !interactive || n + held == 0 || !confirm(&question) {
            println!("Nothing was changed; pass --yes to delete them.");
            return Ok(());
        }
    }

    let report = storage
        .finish_deferred_openebs_deletes(&marked, Duration::from_secs(wait_secs))
        .await?;
    if json_output {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "dry_run": false,
                "reclaimed": report.reclaimed,
                "still_present": report.pending,
                "kept": report.left,
            }))?
        );
        return Ok(());
    }
    for v in &report.reclaimed {
        let held = before
            .iter()
            .find(|a| &a.volume == v)
            .and_then(|a| a.used_bytes);
        println!("reclaimed {v:<42} {:>10}", mib(held));
    }
    for (v, why) in &report.pending {
        println!("still present {v}: {why}");
    }
    for (v, why) in &report.left {
        println!("kept {v}: {why}");
    }
    println!(
        "\n{} reclaimed, {} still present, {} kept.",
        report.reclaimed.len(),
        report.pending.len(),
        report.left.len()
    );
    Ok(())
}
