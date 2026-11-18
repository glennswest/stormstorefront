//! The small roles: the sidecars and init containers around the main ones.
//! Each is what a shell script in a ConfigMap does in the original; here
//! everything is Rust, so the scripts are code and the ConfigMaps hold only
//! configuration.

use crate::http;
use crate::util;
use std::fs;
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// `access-log`: `tail -n +1 -F` the API's file-based access log onto stdout,
/// following truncation and replacement, for the cluster's log collector.
pub fn access_log() -> std::io::Result<()> {
    let path = PathBuf::from(util::env_or("ACCESS_LOG", "/var/log/api/access.log"));
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    fs::OpenOptions::new().create(true).append(true).open(&path)?;
    println!("[access-log] following {}", path.display());
    let mut out = std::io::stdout();
    let mut file = fs::File::open(&path)?;
    let mut ino = file.metadata()?.ino();
    let mut pos = 0u64;
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n > 0 {
            out.write_all(&buf[..n])?;
            out.flush()?;
            pos += n as u64;
            continue;
        }
        std::thread::sleep(Duration::from_millis(250));
        match fs::metadata(&path) {
            Ok(m) if m.ino() != ino => {
                // Replaced (rotated): read the new file from its start.
                if let Ok(f) = fs::File::open(&path) {
                    file = f;
                    ino = m.ino();
                    pos = 0;
                }
            }
            Ok(m) if m.len() < pos => {
                // Truncated in place.
                file.seek(SeekFrom::Start(0))?;
                pos = 0;
            }
            _ => {}
        }
    }
}

/// `cache-warmer`: keeps this pod's catalog entry warm in the cache, so a
/// burst after an expiry does not stampede the store.
pub fn cache_warmer() -> std::io::Result<()> {
    let url = format!("http://127.0.0.1:{}/api/products", util::env_num("API_PORT", 8081u16));
    let interval = util::env_num("WARM_INTERVAL", 20u64);
    std::thread::sleep(Duration::from_secs(util::env_num("WARM_DELAY", 5u64)));
    loop {
        match http::call("GET", &url, &[("User-Agent", "storefront-cache-warmer")], b"", Duration::from_secs(5)) {
            Ok(r) if r.status == 200 => {
                let source = serde_json::from_slice::<serde_json::Value>(&r.body)
                    .ok()
                    .and_then(|v| v["source"].as_str().map(str::to_string))
                    .unwrap_or_else(|| "unknown".into());
                println!("[warm] ok {source}");
            }
            Ok(r) => eprintln!("[warm] failed: status {}", r.status),
            Err(e) => eprintln!("[warm] failed: {e}"),
        }
        std::thread::sleep(Duration::from_secs(interval.max(1)));
    }
}

/// `wait-for-deps` (init): blocks until the store and the cache accept TCP,
/// so a slow data tier is a pending pod rather than a CrashLoopBackOff.
pub fn wait_for_deps() -> std::io::Result<()> {
    let deadline = Instant::now() + Duration::from_secs(util::env_num("DEP_TIMEOUT", 120u64));
    let deps = [
        (util::service_host(&util::env_or("PGHOST", "postgres")), util::env_num("PGPORT", 5432u16)),
        (util::service_host(&util::env_or("REDIS_HOST", "cache")), util::env_num("REDIS_PORT", 6379u16)),
    ];
    for (host, port) in &deps {
        while !http::tcp_reachable(host, *port, Duration::from_secs(2)) {
            if Instant::now() >= deadline {
                eprintln!("[wait] timed out waiting for {host}:{port}");
                return Err(std::io::Error::new(std::io::ErrorKind::TimedOut, format!("{host}:{port}")));
            }
            println!("[wait] {host}:{port} not ready yet");
            std::thread::sleep(Duration::from_secs(2));
        }
        println!("[wait] {host}:{port} is up");
    }
    println!("[wait] all dependencies reachable");
    Ok(())
}

/// Removes all but the newest `keep` backups (names sort by time).
pub fn prune(dir: &Path, keep: usize) -> std::io::Result<Vec<PathBuf>> {
    let mut dumps: Vec<PathBuf> = fs::read_dir(dir)?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "json") && p.file_name().is_some_and(|n| n.to_string_lossy().starts_with("storefront-")))
        .collect();
    dumps.sort();
    let excess = dumps.len().saturating_sub(keep);
    let old: Vec<PathBuf> = dumps.into_iter().take(excess).collect();
    for p in &old {
        fs::remove_file(p)?;
    }
    Ok(old)
}

/// One backup: the store's `/v1/dump`, over loopback, into `dir`.
pub fn backup_once(store_url: &str, token: Option<&str>, dir: &Path) -> std::io::Result<PathBuf> {
    let auth = token.map(|t| format!("Bearer {t}"));
    let headers: Vec<(&str, &str)> = auth.iter().map(|a| ("Authorization", a.as_str())).collect();
    let r = http::call("GET", &format!("{store_url}/v1/dump"), &headers, b"", Duration::from_secs(30))?;
    if r.status != 200 {
        return Err(std::io::Error::other(format!("dump answered {}", r.status)));
    }
    let target = dir.join(format!("storefront-{}.json", util::stamp(util::unix_now())));
    crate::store::write_atomic(&target, &r.body)?;
    Ok(target)
}

/// `backup`: a periodic logical backup to its own PVC, keeping the newest
/// `BACKUP_KEEP`. Shares the pod with the store, so it reads it over loopback
/// and never crosses a NetworkPolicy.
pub fn backup() -> std::io::Result<()> {
    let dir = PathBuf::from(util::env_or("BACKUP_DIR", "/backups"));
    let interval = util::env_num("BACKUP_INTERVAL", 3600u64);
    let keep = util::env_num("BACKUP_KEEP", 6usize);
    let url = format!("http://127.0.0.1:{}", util::env_num("PGPORT", 5432u16));
    let token = util::env_opt("STORE_PASSWORD");
    fs::create_dir_all(&dir)?;
    // Let the store finish starting before the first dump.
    std::thread::sleep(Duration::from_secs(util::env_num("BACKUP_DELAY", 30u64)));
    loop {
        match backup_once(&url, token.as_deref(), &dir) {
            Ok(p) => println!("[backup] wrote {} ({} bytes)", p.display(), fs::metadata(&p).map(|m| m.len()).unwrap_or(0)),
            Err(e) => eprintln!("[backup] FAILED: {e}"),
        }
        match prune(&dir, keep) {
            Ok(old) => old.iter().for_each(|p| println!("[backup] pruned {}", p.display())),
            Err(e) => eprintln!("[backup] prune failed: {e}"),
        }
        std::thread::sleep(Duration::from_secs(interval.max(1)));
    }
}

/// `validate-config` (init): checks the edge's ConfigMap and stages it into
/// the writable volume the edge reads. A bad config stops the rollout here,
/// before anything binds a port, instead of taking running replicas down.
pub fn validate_config() -> std::io::Result<()> {
    let src = PathBuf::from(util::env_or("EDGE_SOURCE", "/etc/edge/src/edge.toml"));
    let live = PathBuf::from(util::env_or("EDGE_CONFIG", "/etc/edge/live/edge.toml"));
    crate::edge::check_file(&src).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    crate::store::write_atomic(&live, &fs::read(&src)?)?;
    println!("[validate-config] {} is valid; staged to {}", src.display(), live.display());
    Ok(())
}

/// The pids of processes running `stormstorefront edge`, as `/proc` shows
/// them: with `shareProcessNamespace` that includes the edge container's.
pub fn edge_pids() -> Vec<i32> {
    let mut pids = Vec::new();
    let me = std::process::id() as i32;
    let Ok(entries) = fs::read_dir("/proc") else { return pids };
    for e in entries.flatten() {
        let Ok(pid) = e.file_name().to_string_lossy().parse::<i32>() else { continue };
        if pid == me {
            continue;
        }
        let Ok(cmdline) = fs::read(e.path().join("cmdline")) else { continue };
        let args: Vec<&[u8]> = cmdline.split(|b| *b == 0).collect();
        let is_us = args.first().is_some_and(|a| a.ends_with(b"stormstorefront"));
        if is_us && args.get(1).is_some_and(|a| *a == b"edge") {
            pids.push(pid);
        }
    }
    pids
}

/// `config-reloader`: watches the ConfigMap mount; on a change, validates,
/// copies it into the live volume and sends the edge SIGHUP — the original's
/// `reload.sh` with its `pkill -USR2`. A rejected config is reported and the
/// running one kept. Should the runtime not share the PID namespace, the copy
/// alone still reloads the edge, which watches its live file.
pub fn config_reloader() -> std::io::Result<()> {
    let src = PathBuf::from(util::env_or("EDGE_SOURCE", "/etc/edge/src/edge.toml"));
    let live = PathBuf::from(util::env_or("EDGE_CONFIG", "/etc/edge/live/edge.toml"));
    let interval = Duration::from_secs(util::env_num("RELOAD_INTERVAL", 10u64));
    let sum = |p: &Path| fs::read(p).map(|d| util::fnv1a(&d)).unwrap_or(0);
    let mut last = sum(&src);
    println!("[reloader] watching {} (checksum {last:016x})", src.display());
    loop {
        std::thread::sleep(interval);
        let now = sum(&src);
        if now == last {
            continue;
        }
        println!("[reloader] change detected: {last:016x} -> {now:016x}");
        last = now;
        if let Err(e) = crate::edge::check_file(&src) {
            println!("[reloader] REJECTED, keeping the running config: {e}");
            continue;
        }
        crate::store::write_atomic(&live, &fs::read(&src)?)?;
        let pids = edge_pids();
        if pids.is_empty() {
            println!("[reloader] staged; no edge process visible (is shareProcessNamespace set?) — the edge picks the file up itself");
        }
        for pid in pids {
            // SAFETY: kill(2) with a pid read from /proc and a valid signal.
            let rc = unsafe { libc::kill(pid, libc::SIGHUP) };
            if rc == 0 {
                println!("[reloader] reload signalled to pid {pid}");
            } else {
                eprintln!("[reloader] signal to pid {pid} failed: {}", std::io::Error::last_os_error());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prune_keeps_the_newest() {
        let dir = std::env::temp_dir().join(format!("storefront-prune-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        for d in ["20260101T000000Z", "20260102T000000Z", "20260103T000000Z"] {
            fs::write(dir.join(format!("storefront-{d}.json")), b"{}").unwrap();
        }
        fs::write(dir.join("other.txt"), b"x").unwrap();
        let old = prune(&dir, 2).unwrap();
        assert_eq!(old.len(), 1);
        assert!(old[0].to_string_lossy().contains("20260101"));
        assert!(dir.join("other.txt").exists());
        let _ = fs::remove_dir_all(&dir);
    }
}
