//! The API tier (`api` role): the endpoints of the original's `src/api/api.py`,
//! in Rust, plus orders.
//!
//! ```text
//! GET  /api/healthz  /healthz     liveness: {"status": "ok", "uptime_s": n}
//! GET  /api/readyz   /readyz      503 while the store or the cache is unreachable
//! GET  /api/products              the catalog, from the cache, else the store
//! POST /api/orders                {"sku": "...", "qty": n} -> 201 and the order
//! GET  /api/orders/<id>
//! GET  /api/metrics               Prometheus text
//! ```
//!
//! Where the original serves an in-process catalog and only TCP-checks
//! postgres, this one reads the catalog from the store and writes orders to
//! it, so the database tier is really on the request path and the test can
//! prove a PVC keeps its data across a pod restart.
//!
//! The cache must never fail a request: every cache call is wrapped, a failure
//! counts in `storefront_api_upstream_errors_total` and falls through to the
//! store, as in the original.

use crate::http::{self, Request, Response};
use crate::resp;
use crate::util;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::io::Write;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

pub const CATALOG_KEY: &str = "storefront:catalog";

pub struct Api {
    pod: String,
    node: String,
    store_url: String,
    store_token: Option<String>,
    store_host: String,
    store_port: u16,
    cache_host: String,
    cache_port: u16,
    cache: resp::Client,
    cache_ttl: u64,
    access_log: String,
    started: Instant,
    requests: AtomicU64,
    by_status: Mutex<BTreeMap<u16, u64>>,
    cache_hits: AtomicU64,
    cache_misses: AtomicU64,
    upstream_errors: AtomicU64,
    orders: AtomicU64,
}

impl Api {
    pub fn from_env() -> Self {
        let store_host = util::service_host(&util::env_or("PGHOST", "postgres"));
        let store_port = util::env_num("PGPORT", 5432u16);
        let cache_host = util::service_host(&util::env_or("REDIS_HOST", "cache"));
        let cache_port = util::env_num("REDIS_PORT", 6379u16);
        Api {
            pod: util::pod_name(),
            node: util::node_name(),
            store_url: format!("http://{store_host}:{store_port}"),
            store_token: util::env_opt("STORE_PASSWORD"),
            cache: resp::Client::new(&cache_host, cache_port, util::env_opt("REDIS_PASSWORD"), Duration::from_millis(1500)),
            store_host,
            store_port,
            cache_host,
            cache_port,
            cache_ttl: util::env_num("CACHE_TTL_SECONDS", 30u64),
            access_log: util::env_or("ACCESS_LOG", "/var/log/api/access.log"),
            started: Instant::now(),
            requests: AtomicU64::new(0),
            by_status: Mutex::new(BTreeMap::new()),
            cache_hits: AtomicU64::new(0),
            cache_misses: AtomicU64::new(0),
            upstream_errors: AtomicU64::new(0),
            orders: AtomicU64::new(0),
        }
    }

    fn store(&self, method: &str, path: &str, body: &[u8]) -> std::io::Result<Response> {
        let auth = self.store_token.as_ref().map(|t| format!("Bearer {t}"));
        let mut headers: Vec<(&str, &str)> = vec![("Content-Type", "application/json")];
        if let Some(a) = &auth {
            headers.push(("Authorization", a.as_str()));
        }
        http::call(method, &format!("{}{path}", self.store_url), &headers, body, Duration::from_secs(3))
    }

    fn upstream_error(&self, what: &str, e: impl std::fmt::Display) {
        self.upstream_errors.fetch_add(1, Ordering::Relaxed);
        eprintln!("[api] {what} failed: {e}");
    }

    /// (payload, source) where source is "cache" or "origin"; None when the
    /// store cannot answer either.
    fn catalog(&self) -> Option<(Value, &'static str)> {
        match self.cache.get(CATALOG_KEY) {
            Ok(Some(raw)) => match serde_json::from_slice(&raw) {
                Ok(v) => {
                    self.cache_hits.fetch_add(1, Ordering::Relaxed);
                    return Some((v, "cache"));
                }
                Err(e) => self.upstream_error("cache decode", e),
            },
            Ok(None) => {}
            Err(e) => self.upstream_error("cache read", e),
        }
        self.cache_misses.fetch_add(1, Ordering::Relaxed);
        let products = match self.store("GET", "/v1/products", b"") {
            Ok(r) if r.status == 200 => match serde_json::from_slice::<Value>(&r.body) {
                Ok(v) => v["products"].clone(),
                Err(e) => {
                    self.upstream_error("store decode", e);
                    return None;
                }
            },
            Ok(r) => {
                self.upstream_error("store read", format!("status {}", r.status));
                return None;
            }
            Err(e) => {
                self.upstream_error("store read", e);
                return None;
            }
        };
        let payload = json!({
            "products": products,
            "count": products.as_array().map(|a| a.len()).unwrap_or(0),
            "generated_at": util::rfc3339(util::unix_now()),
        });
        if let Err(e) = self.cache.setex(CATALOG_KEY, self.cache_ttl, payload.to_string().as_bytes()) {
            self.upstream_error("cache write", e);
        }
        Some((payload, "origin"))
    }

    fn reply(&self, status: u16, mut payload: Value, source: Option<&str>) -> Response {
        if let Some(obj) = payload.as_object_mut() {
            obj.insert("served_by".into(), json!({"pod": self.pod, "node": self.node}));
            if let Some(s) = source {
                obj.insert("source".into(), json!(s));
            }
        }
        Response::json(status, &payload)
    }

    fn forward_store(&self, method: &str, path: &str, body: &[u8]) -> Response {
        match self.store(method, path, body) {
            Ok(r) if r.status < 500 => {
                if method == "POST" && r.status == 201 {
                    self.orders.fetch_add(1, Ordering::Relaxed);
                }
                let v: Value = serde_json::from_slice(&r.body).unwrap_or_else(|_| json!({"raw": r.body_str()}));
                self.reply(r.status, v, Some("origin"))
            }
            Ok(r) => {
                self.upstream_error("store", format!("status {}", r.status));
                self.reply(503, json!({"error": "store unavailable"}), None)
            }
            Err(e) => {
                self.upstream_error("store", e);
                self.reply(503, json!({"error": "store unavailable"}), None)
            }
        }
    }

    pub fn route(&self, req: &Request) -> Response {
        let trimmed = req.path().trim_end_matches('/');
        let path = if trimmed.is_empty() { "/" } else { trimmed };
        match (req.method.as_str(), path) {
            ("GET" | "HEAD", "/api/healthz" | "/healthz") => {
                self.reply(200, json!({"status": "ok", "uptime_s": self.started.elapsed().as_secs()}), None)
            }
            ("GET" | "HEAD", "/api/readyz" | "/readyz") => {
                let t = Duration::from_millis(1500);
                let pg = http::tcp_reachable(&self.store_host, self.store_port, t);
                let redis = http::tcp_reachable(&self.cache_host, self.cache_port, t);
                let ok = pg && redis;
                // The original's dependency names, so its smoke test reads the same.
                self.reply(
                    if ok { 200 } else { 503 },
                    json!({"status": if ok { "ready" } else { "degraded" }, "dependencies": {"postgres": pg, "redis": redis}}),
                    None,
                )
            }
            ("GET" | "HEAD", "/api/products") => match self.catalog() {
                Some((payload, source)) => self.reply(200, payload, Some(source)),
                None => self.reply(503, json!({"error": "catalog unavailable"}), None),
            },
            ("POST", "/api/orders") => self.forward_store("POST", "/v1/orders", &req.body),
            ("GET", p) if p.starts_with("/api/orders/") => {
                let id = &p["/api/orders/".len()..];
                if id.chars().all(|c| c.is_ascii_digit()) && !id.is_empty() {
                    self.forward_store("GET", &format!("/v1/orders/{id}"), b"")
                } else {
                    self.reply(400, json!({"error": "order ids are numbers"}), None)
                }
            }
            ("GET" | "HEAD", "/api/metrics") => Response::new(200, "text/plain; version=0.0.4", self.metrics().into_bytes()),
            (m, _) if m != "GET" && m != "HEAD" && m != "POST" => self.reply(405, json!({"error": "method not allowed"}), None),
            _ => self.reply(404, json!({"error": "not found", "path": path}), None),
        }
    }

    pub fn handle(&self, req: Request) -> Response {
        let resp = self.route(&req).with_header("X-Api-Pod", &self.pod);
        self.requests.fetch_add(1, Ordering::Relaxed);
        *self.by_status.lock().unwrap_or_else(|p| p.into_inner()).entry(resp.status).or_insert(0) += 1;
        self.log(&req, &resp);
        resp
    }

    /// A combined-style line in the file the `access-log` sidecar tails.
    fn log(&self, req: &Request, resp: &Response) {
        let peer = req.peer.map(|p| p.ip().to_string()).unwrap_or_else(|| "-".into());
        let line = format!(
            "{peer} - - [{}] \"{} {} {}\" {} {} \"{}\" {}\n",
            util::clf_time(util::unix_now()),
            req.method,
            req.target,
            req.version,
            resp.status,
            resp.body.len(),
            req.header("user-agent").unwrap_or("-"),
            self.pod
        );
        let written = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.access_log)
            .and_then(|mut f| f.write_all(line.as_bytes()));
        if written.is_err() {
            eprint!("{line}");
        }
    }

    pub fn metrics(&self) -> String {
        let pod = util::label(&self.pod);
        let mut out = String::new();
        let mut metric = |name: &str, kind: &str, help: &str, value: u64| {
            out.push_str(&format!("# HELP {name} {help}\n# TYPE {name} {kind}\n{name}{{pod=\"{pod}\"}} {value}\n"));
        };
        metric("storefront_api_requests_total", "counter", "Requests handled by this pod.", self.requests.load(Ordering::Relaxed));
        metric("storefront_api_cache_hits_total", "counter", "Catalog reads served from the cache.", self.cache_hits.load(Ordering::Relaxed));
        metric("storefront_api_cache_misses_total", "counter", "Catalog reads that missed the cache.", self.cache_misses.load(Ordering::Relaxed));
        metric("storefront_api_upstream_errors_total", "counter", "Failed upstream calls.", self.upstream_errors.load(Ordering::Relaxed));
        metric("storefront_api_orders_total", "counter", "Orders created through this pod.", self.orders.load(Ordering::Relaxed));
        metric("storefront_api_uptime_seconds", "gauge", "Seconds since process start.", self.started.elapsed().as_secs());
        out.push_str("# HELP storefront_api_responses_total Responses by HTTP status.\n# TYPE storefront_api_responses_total counter\n");
        for (status, n) in self.by_status.lock().unwrap_or_else(|p| p.into_inner()).iter() {
            out.push_str(&format!("storefront_api_responses_total{{pod=\"{pod}\",status=\"{status}\"}} {n}\n"));
        }
        out
    }
}

pub fn run() -> std::io::Result<()> {
    let api = Api::from_env();
    if let Some(dir) = std::path::Path::new(&api.access_log).parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let listen = format!(":{}", util::env_num("API_PORT", 8081u16));
    println!(
        "[api] listening on {listen} pod={} node={} cache={}:{} store={}:{}",
        api.pod, api.node, api.cache_host, api.cache_port, api.store_host, api.store_port
    );
    http::serve(&listen, move |req| api.handle(req))
}
