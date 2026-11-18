//! The `exporter` sidecar: scrapes its pod neighbour's loopback-only counters
//! and serves them as Prometheus text on the pod's metrics port — the role
//! nginx-prometheus-exporter, postgres_exporter and redis_exporter play in the
//! original. One implementation for all three tiers:
//!
//! ```text
//! exporter --scrape http://127.0.0.1:8081/stub_status --listen :9113 --prefix storefront_web
//! ```
//!
//! The neighbour answers `name value` lines; each becomes
//! `<prefix>_<name>{pod="…"} value`. `<prefix>_up` is 1 when the scrape
//! worked and 0 when it did not (the exporter itself stays up, so the gap is
//! visible rather than a missing target).

use crate::http::{self, Response};
use crate::util;
use std::time::Duration;

pub struct Exporter {
    pub scrape: String,
    pub prefix: String,
    pub pod: String,
}

/// Turns `name value` lines into Prometheus text. Names ending in `_total`
/// are counters, the rest gauges. The neighbour's own `up` line is dropped:
/// `<prefix>_up` says whether the scrape worked.
pub fn convert(prefix: &str, pod: &str, body: &str) -> String {
    let pod = util::label(pod);
    let mut out = String::new();
    for line in body.lines() {
        let mut parts = line.split_whitespace();
        let (Some(name), Some(value), None) = (parts.next(), parts.next(), parts.next()) else {
            continue;
        };
        if name == "up" || value.parse::<f64>().is_err() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            continue;
        }
        let kind = if name.ends_with("_total") { "counter" } else { "gauge" };
        out.push_str(&format!("# TYPE {prefix}_{name} {kind}\n{prefix}_{name}{{pod=\"{pod}\"}} {value}\n"));
    }
    out
}

impl Exporter {
    pub fn metrics(&self) -> String {
        let pod = util::label(&self.pod);
        let (up, body) = match http::get(&self.scrape, Duration::from_secs(3)) {
            Ok(r) if r.status == 200 => (1, convert(&self.prefix, &self.pod, &r.body_str())),
            Ok(r) => {
                eprintln!("[exporter] {} answered {}", self.scrape, r.status);
                (0, String::new())
            }
            Err(e) => {
                eprintln!("[exporter] {}: {e}", self.scrape);
                (0, String::new())
            }
        };
        format!(
            "# HELP {p}_up Whether the last scrape of the pod's neighbour worked.\n# TYPE {p}_up gauge\n{p}_up{{pod=\"{pod}\"}} {up}\n{body}",
            p = self.prefix
        )
    }
}

fn flag(args: &[String], name: &str) -> Option<String> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned().or_else(|| {
        let prefix = format!("{name}=");
        args.iter().find_map(|a| a.strip_prefix(&prefix).map(str::to_string))
    })
}

pub fn run(args: &[String]) -> std::io::Result<()> {
    let need = |name: &str| {
        flag(args, name).ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, format!("exporter needs {name}")))
    };
    let exporter = Exporter { scrape: need("--scrape")?, prefix: need("--prefix")?, pod: util::pod_name() };
    let listen = flag(args, "--listen").unwrap_or_else(|| ":9100".into());
    println!("[exporter] {} -> {listen}/metrics as {}_*", exporter.scrape, exporter.prefix);
    http::serve(&listen, move |req| match req.path() {
        "/metrics" => Response::new(200, "text/plain; version=0.0.4", exporter.metrics().into_bytes()),
        "/health" | "/healthz" | "/" => Response::text(200, "ok\n"),
        _ => Response::text(404, "not found\n"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_lines() {
        let out = convert("storefront_cache", "cache-1", "up 1\nkeys 3\ncommands_total 10\nbad line here\nx notanumber\n");
        assert!(out.contains("# TYPE storefront_cache_keys gauge\nstorefront_cache_keys{pod=\"cache-1\"} 3\n"));
        assert!(out.contains("# TYPE storefront_cache_commands_total counter\n"));
        assert!(!out.contains("storefront_cache_up"));
        assert!(!out.contains("bad"));
    }

    #[test]
    fn flags_both_spellings() {
        let a: Vec<String> = ["--scrape", "http://x", "--prefix=p"].iter().map(|s| s.to_string()).collect();
        assert_eq!(flag(&a, "--scrape").as_deref(), Some("http://x"));
        assert_eq!(flag(&a, "--prefix").as_deref(), Some("p"));
        assert_eq!(flag(&a, "--listen"), None);
    }
}
