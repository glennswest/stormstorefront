//! The content renderer: `content-init` (once, as the web pod's init
//! container, so the pod is never Ready with a 404) and `content-sync` (every
//! `SYNC_INTERVAL` seconds, as a sidecar). Stands in for "pull the built site
//! from object storage", as the original's `sync-content.sh` does.

use crate::store::write_atomic;
use crate::util;
use std::path::Path;
use std::time::Duration;

fn escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

pub fn index_html(pod: &str, node: &str, rev: u64, rendered: &str) -> String {
    format!(
        r#"<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <title>Storefront</title>
  <style>
    :root {{ color-scheme: light dark; }}
    body {{ font: 16px/1.5 ui-sans-serif, system-ui, sans-serif; margin: 3rem auto; max-width: 42rem; padding: 0 1rem; }}
    code {{ background: rgba(127,127,127,.18); padding: .1em .35em; border-radius: .25em; }}
    dt {{ font-weight: 600; margin-top: .75rem; }}
  </style>
</head>
<body>
  <h1>Storefront</h1>
  <p>Static content served by the web tier, on stormcos.</p>
  <dl>
    <dt>Pod</dt><dd><code>{pod}</code></dd>
    <dt>Node</dt><dd><code>{node}</code></dd>
    <dt>Content revision</dt><dd><code>{rev}</code> (rendered {rendered})</dd>
  </dl>
  <p>Backend: <a href="/api/products">/api/products</a> &middot;
     <a href="/api/healthz">/api/healthz</a> &middot;
     <a href="/api/readyz">/api/readyz</a></p>
</body>
</html>
"#,
        pod = escape(pod),
        node = escape(node),
    )
}

/// Renders `index.html` and `version.json` into `dest`, each atomically.
pub fn render(dest: &Path, pod: &str, node: &str) -> std::io::Result<u64> {
    let rev = util::unix_now();
    let rendered = util::rfc3339(rev);
    write_atomic(&dest.join("index.html"), index_html(pod, node, rev, &rendered).as_bytes())?;
    let version = serde_json::json!({"revision": rev.to_string(), "rendered": rendered, "pod": pod});
    write_atomic(&dest.join("version.json"), format!("{version}\n").as_bytes())?;
    Ok(rev)
}

pub fn run(oneshot: bool) -> std::io::Result<()> {
    let dest = std::path::PathBuf::from(util::env_or("DEST", "/srv/www"));
    let interval = util::env_num("SYNC_INTERVAL", 30u64);
    let (pod, node) = (util::pod_name(), util::node_name());
    std::fs::create_dir_all(&dest)?;
    render(&dest, &pod, &node)?;
    println!("[content] initial render complete in {}", dest.display());
    if oneshot {
        return Ok(());
    }
    loop {
        std::thread::sleep(Duration::from_secs(interval.max(1)));
        match render(&dest, &pod, &node) {
            Ok(rev) => println!("[content] refreshed revision {rev}"),
            Err(e) => eprintln!("[content] render failed: {e}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_both_files() {
        let dir = std::env::temp_dir().join(format!("storefront-content-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        render(&dir, "web-<x>", "node1").unwrap();
        let index = std::fs::read_to_string(dir.join("index.html")).unwrap();
        assert!(index.contains("Storefront") && index.contains("web-&lt;x&gt;"));
        let v: serde_json::Value = serde_json::from_slice(&std::fs::read(dir.join("version.json")).unwrap()).unwrap();
        assert!(v["revision"].is_string());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
