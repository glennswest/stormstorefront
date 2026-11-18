//! Small helpers every role uses: environment, time, service names.

use std::time::{SystemTime, UNIX_EPOCH};

pub fn env_or(name: &str, default: &str) -> String {
    match std::env::var(name) {
        Ok(v) if !v.is_empty() => v,
        _ => default.to_string(),
    }
}

pub fn env_num<T: std::str::FromStr>(name: &str, default: T) -> T {
    std::env::var(name).ok().and_then(|v| v.trim().parse().ok()).unwrap_or(default)
}

pub fn env_opt(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

/// The pod's name (downward API), else the hostname, else "unknown".
pub fn pod_name() -> String {
    env_opt("POD_NAME")
        .or_else(|| env_opt("HOSTNAME"))
        .or_else(|| std::fs::read_to_string("/etc/hostname").ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty()))
        .unwrap_or_else(|| "unknown".to_string())
}

pub fn node_name() -> String {
    env_or("NODE_NAME", "unknown")
}

/// A Service name as a fully qualified in-cluster DNS name.
///
/// A bare name (`postgres`) becomes `postgres.<POD_NAMESPACE>.svc.<CLUSTER_DOMAIN>`
/// so the manifests never hard-code their project: the test deploys the same
/// manifests into its own project. A name with a dot, or an IP, is kept.
pub fn service_host(name: &str) -> String {
    if name.contains('.') || name.contains(':') || name == "localhost" {
        return name.to_string();
    }
    match env_opt("POD_NAMESPACE") {
        Some(ns) => format!("{name}.{ns}.svc.{}", env_or("CLUSTER_DOMAIN", "cluster.local")),
        None => name.to_string(),
    }
}

pub fn unix_now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// Days since 1970-01-01 → (year, month, day), Howard Hinnant's algorithm.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

fn split(secs: u64) -> (i64, u32, u32, u64, u64, u64) {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let (y, m, d) = civil_from_days(days);
    (y, m, d, rem / 3600, (rem % 3600) / 60, rem % 60)
}

/// `2026-11-18T07:43:00Z`
pub fn rfc3339(secs: u64) -> String {
    let (y, m, d, hh, mm, ss) = split(secs);
    format!("{y:04}-{m:02}-{d:02}T{hh:02}:{mm:02}:{ss:02}Z")
}

/// `20261118T074300Z`, sortable, for file names.
pub fn stamp(secs: u64) -> String {
    let (y, m, d, hh, mm, ss) = split(secs);
    format!("{y:04}{m:02}{d:02}T{hh:02}{mm:02}{ss:02}Z")
}

/// `18/Nov/2026:07:43:00 +0000`, the combined log format's time.
pub fn clf_time(secs: u64) -> String {
    const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    let (y, m, d, hh, mm, ss) = split(secs);
    format!("{d:02}/{}/{y:04}:{hh:02}:{mm:02}:{ss:02} +0000", MONTHS[(m - 1) as usize])
}

/// `host:port` → (host, port), with a default port.
pub fn host_port(spec: &str, default_port: u16) -> (String, u16) {
    match spec.rsplit_once(':') {
        Some((h, p)) => match p.parse() {
            Ok(port) => (h.to_string(), port),
            Err(_) => (spec.to_string(), default_port),
        },
        None => (spec.to_string(), default_port),
    }
}

/// FNV-1a, for config checksums and cookie values (not security).
pub fn fnv1a(data: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in data {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    h
}

/// Escapes a Prometheus label value.
pub fn label(v: &str) -> String {
    v.replace('\\', "\\\\").replace('"', "\\\"").replace('\n', "\\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn times_format() {
        // 2026-11-18T07:43:00Z
        let t = 1_794_987_780;
        assert_eq!(rfc3339(t), "2026-11-18T07:43:00Z");
        assert_eq!(stamp(t), "20261118T074300Z");
        assert_eq!(clf_time(t), "18/Nov/2026:07:43:00 +0000");
        assert_eq!(rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339(951_782_400), "2000-02-29T00:00:00Z");
    }

    #[test]
    fn host_ports() {
        assert_eq!(host_port("postgres:5432", 1), ("postgres".into(), 5432));
        assert_eq!(host_port("cache", 6379), ("cache".into(), 6379));
    }
}
