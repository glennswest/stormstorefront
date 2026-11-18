//! The web tier (`web` role), standing where the original runs nginx:
//! serves the rendered site from the shared `content` volume on :8080 and
//! proxies `/api/` to the API Service, so a pod reached directly behaves as it
//! does through the edge.
//!
//! `/healthz` is liveness; `/readyz` is 503 until the content sidecar has
//! produced an index. A second listener on loopback only (`STATS_LISTEN`,
//! default 127.0.0.1:8081, `/stub_status`) gives the `exporter` sidecar its
//! counters, as nginx's `stub_status` vhost does.

use crate::http::{self, Request, Response};
use crate::util;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

pub struct Web {
    root: PathBuf,
    api_url: String,
    pod: String,
    started: Instant,
    pub active: AtomicU64,
    pub requests: AtomicU64,
    pub proxied: AtomicU64,
    pub proxy_errors: AtomicU64,
    pub not_found: AtomicU64,
}

pub fn content_type(path: &Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()).unwrap_or("") {
        "html" | "htm" => "text/html; charset=utf-8",
        "json" => "application/json",
        "css" => "text/css",
        "js" => "application/javascript",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "txt" => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

/// A request path as a file under `root`, refusing anything that climbs out.
pub fn safe_join(root: &Path, url_path: &str) -> Option<PathBuf> {
    let mut out = root.to_path_buf();
    for c in Path::new(url_path.trim_start_matches('/')).components() {
        match c {
            Component::Normal(p) => out.push(p),
            Component::CurDir => {}
            _ => return None,
        }
    }
    Some(out)
}

impl Web {
    pub fn new(root: PathBuf, api_url: String, pod: String) -> Self {
        Web {
            root,
            api_url,
            pod,
            started: Instant::now(),
            active: AtomicU64::new(0),
            requests: AtomicU64::new(0),
            proxied: AtomicU64::new(0),
            proxy_errors: AtomicU64::new(0),
            not_found: AtomicU64::new(0),
        }
    }

    fn proxy(&self, req: &Request) -> Response {
        self.proxied.fetch_add(1, Ordering::Relaxed);
        let peer = req.peer.map(|p| p.ip().to_string()).unwrap_or_default();
        let xff = match req.header("x-forwarded-for") {
            Some(prev) => format!("{prev}, {peer}"),
            None => peer.clone(),
        };
        let mut headers: Vec<(&str, &str)> = Vec::new();
        for (k, v) in &req.headers {
            if !http::is_hop_by_hop(k) && !k.eq_ignore_ascii_case("host") && !k.eq_ignore_ascii_case("x-forwarded-for") {
                headers.push((k.as_str(), v.as_str()));
            }
        }
        headers.push(("X-Forwarded-For", &xff));
        headers.push(("X-Real-IP", &peer));
        match http::call(&req.method, &format!("{}{}", self.api_url, req.target), &headers, &req.body, Duration::from_secs(15)) {
            Ok(mut r) => {
                r.headers.retain(|(k, _)| !http::is_hop_by_hop(k));
                r
            }
            Err(e) => {
                self.proxy_errors.fetch_add(1, Ordering::Relaxed);
                eprintln!("[web] proxy {} {}: {e}", req.method, req.target);
                Response::text(502, "api unavailable\n")
            }
        }
    }

    fn file(&self, req: &Request) -> Response {
        let path = req.path();
        let index = self.root.join("index.html");
        let candidate = match safe_join(&self.root, path) {
            Some(p) if p.is_dir() => p.join("index.html"),
            Some(p) => p,
            None => return Response::text(400, "bad path\n"),
        };
        // try_files $uri $uri/ /index.html
        let chosen = if candidate.is_file() { candidate } else { index };
        match std::fs::read(&chosen) {
            Ok(body) => Response::new(200, content_type(&chosen), body).with_header("Cache-Control", "no-cache"),
            Err(_) => {
                self.not_found.fetch_add(1, Ordering::Relaxed);
                Response::text(404, "not found\n")
            }
        }
    }

    pub fn handle(&self, req: &Request) -> Response {
        self.requests.fetch_add(1, Ordering::Relaxed);
        self.active.fetch_add(1, Ordering::Relaxed);
        let resp = match req.path() {
            "/healthz" => Response::text(200, "ok\n"),
            "/readyz" => {
                if self.root.join("index.html").is_file() {
                    Response::text(200, "ready\n")
                } else {
                    Response::text(503, "no content\n")
                }
            }
            p if p == "/api" || p.starts_with("/api/") => self.proxy(req),
            _ if req.method == "GET" || req.method == "HEAD" => self.file(req),
            _ => Response::text(405, "method not allowed\n"),
        };
        self.active.fetch_sub(1, Ordering::Relaxed);
        resp.with_header("X-Web-Pod", &self.pod)
    }

    /// `name value` lines for the exporter (nginx's stub_status, as numbers).
    pub fn stub_status(&self) -> String {
        format!(
            "up 1\nuptime_seconds {}\nconnections_active {}\nhttp_requests_total {}\nproxied_requests_total {}\nproxy_errors_total {}\nnot_found_total {}\ncontent_ready {}\n",
            self.started.elapsed().as_secs(),
            self.active.load(Ordering::Relaxed),
            self.requests.load(Ordering::Relaxed),
            self.proxied.load(Ordering::Relaxed),
            self.proxy_errors.load(Ordering::Relaxed),
            self.not_found.load(Ordering::Relaxed),
            u8::from(self.root.join("index.html").is_file()),
        )
    }
}

pub fn run() -> std::io::Result<()> {
    let root = PathBuf::from(util::env_or("CONTENT_DIR", "/srv/www"));
    let api_host = util::service_host(&util::env_or("API_HOST", "api"));
    let api_url = format!("http://{api_host}:{}", util::env_num("API_PORT", 8081u16));
    let web = Arc::new(Web::new(root.clone(), api_url.clone(), util::pod_name()));
    {
        let web = Arc::clone(&web);
        http::spawn_server(&util::env_or("STATS_LISTEN", "127.0.0.1:8081"), move |req| match req.path() {
            "/stub_status" => Response::text(200, &web.stub_status()),
            _ => Response::text(404, "not found\n"),
        })?;
    }
    let listen = util::env_or("WEB_LISTEN", ":8080");
    println!("[web] serving {} on {listen}, /api/ -> {api_url}", root.display());
    http::serve(&listen, move |req| web.handle(&req))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_stay_inside_the_root() {
        let root = Path::new("/srv/www");
        assert_eq!(safe_join(root, "/a/b.html"), Some(PathBuf::from("/srv/www/a/b.html")));
        assert_eq!(safe_join(root, "/../etc/passwd"), None);
        assert_eq!(safe_join(root, "/"), Some(PathBuf::from("/srv/www")));
    }

    #[test]
    fn serves_index_and_readiness() {
        let dir = std::env::temp_dir().join(format!("storefront-web-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let web = Web::new(dir.clone(), "http://127.0.0.1:1".into(), "web-0".into());
        assert_eq!(web.handle(&Request::new("GET", "/readyz")).status, 503);
        std::fs::write(dir.join("index.html"), "<h1>Storefront</h1>").unwrap();
        assert_eq!(web.handle(&Request::new("GET", "/readyz")).status, 200);
        let r = web.handle(&Request::new("GET", "/nope"));
        assert_eq!(r.status, 200);
        assert!(r.body_str().contains("Storefront"));
        assert_eq!(r.header("X-Web-Pod"), Some("web-0"));
        // The API is down here: a 502, never a hang.
        assert_eq!(web.handle(&Request::new("GET", "/api/products")).status, 502);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
