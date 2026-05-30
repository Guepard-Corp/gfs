//! Proxy telemetry: scrape the warming proxy's Prometheus `/metrics` and its
//! live `/clones` discovery map. Both are served on the proxy's metrics port
//! (default `127.0.0.1:9090`); see `crates/applications/proxy/src/discovery.rs`.

use serde::Serialize;

/// One parsed Prometheus sample (only `proxy_*` series are kept).
#[derive(Debug, Clone, Serialize)]
pub struct Metric {
    pub name: String,
    pub labels: std::collections::BTreeMap<String, String>,
    pub value: f64,
}

#[derive(Debug, Serialize)]
pub struct TelemetrySnapshot {
    pub proxy_url: String,
    pub reachable: bool,
    pub error: Option<String>,
    pub metrics: Vec<Metric>,
    /// Raw `/clones` payload (`{ "clones": [...] }`) when the proxy runs `--discover`.
    pub clones: serde_json::Value,
}

/// Fetch + parse proxy telemetry. Never errors: an unreachable proxy yields
/// `reachable=false` so the dashboard can render a degraded state.
pub async fn snapshot(client: &reqwest::Client, proxy_url: &str) -> TelemetrySnapshot {
    let base = proxy_url.trim_end_matches('/');

    let metrics_res = client.get(format!("{base}/metrics")).send().await;
    match metrics_res {
        Ok(resp) => match resp.text().await {
            Ok(body) => {
                let clones = fetch_clones(client, base).await;
                TelemetrySnapshot {
                    proxy_url: base.to_string(),
                    reachable: true,
                    error: None,
                    metrics: parse_prometheus(&body),
                    clones,
                }
            }
            Err(e) => unreachable_snapshot(base, e.to_string()),
        },
        Err(e) => unreachable_snapshot(base, e.to_string()),
    }
}

async fn fetch_clones(client: &reqwest::Client, base: &str) -> serde_json::Value {
    match client.get(format!("{base}/clones")).send().await {
        Ok(resp) => resp.json::<serde_json::Value>().await.unwrap_or_else(|_| {
            serde_json::json!({ "clones": [] })
        }),
        Err(_) => serde_json::json!({ "clones": [] }),
    }
}

fn unreachable_snapshot(base: &str, error: String) -> TelemetrySnapshot {
    TelemetrySnapshot {
        proxy_url: base.to_string(),
        reachable: false,
        error: Some(error),
        metrics: Vec::new(),
        clones: serde_json::json!({ "clones": [] }),
    }
}

/// Minimal Prometheus text-format parser, keeping only `proxy_*` series.
/// Handles `name value` and `name{k="v",...} value`.
fn parse_prometheus(body: &str) -> Vec<Metric> {
    let mut out = Vec::new();
    for line in body.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if !line.starts_with("proxy_") {
            continue;
        }
        let Some((series, value_str)) = line.rsplit_once(' ') else {
            continue;
        };
        let Ok(value) = value_str.trim().parse::<f64>() else {
            continue;
        };
        let (name, labels) = match series.split_once('{') {
            Some((name, rest)) => {
                let rest = rest.trim_end_matches('}');
                (name.to_string(), parse_labels(rest))
            }
            None => (series.to_string(), Default::default()),
        };
        out.push(Metric { name, labels, value });
    }
    out
}

fn parse_labels(s: &str) -> std::collections::BTreeMap<String, String> {
    let mut map = std::collections::BTreeMap::new();
    for pair in s.split(',') {
        let pair = pair.trim();
        if pair.is_empty() {
            continue;
        }
        if let Some((k, v)) = pair.split_once('=') {
            map.insert(
                k.trim().to_string(),
                v.trim().trim_matches('"').to_string(),
            );
        }
    }
    map
}
