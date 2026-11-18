//! The edge (`edge` role), standing where the original runs HAProxy 2.9: the
//! L7 load balancer in front of everything.
//!
//! - answers `/healthz` itself, without touching a backend;
//! - stamps `X-Request-Id` (unless the client sent one) and `X-Served-By`;
//! - routes by path prefix (`/api/` → `api`, everything else → `web`);
//! - balances across **pod IPs**: each backend names a *headless* Service, the
//!   resolver keeps one server per ready pod (up to `slots`), and the edge
//!   health-checks each itself (`rise`/`fall`), as HAProxy's
//!   `server-template … resolvers kube` does — so `leastconn`, cookie
//!   affinity and the per-pod `X-Served-By` mean something;
//! - retries another server on a refused connection (any method) or a failed
//!   exchange (GET/HEAD only);
//! - reloads its config seamlessly: on SIGHUP (the `config-reloader` sidecar
//!   sends it through the pod's shared PID namespace) and whenever the live
//!   file changes (so a runtime without a shared PID namespace still
//!   reloads). A config that does not validate is refused and the running one
//!   kept.
//!
//! Stats and Prometheus text on `metrics_listen` (`/metrics`, `/stats`), a
//! port deliberately not on the LoadBalancer Service.

use crate::http::{self, Request, Response};
use crate::util;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::net::{SocketAddr, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

fn default_health() -> String {
    "/healthz".into()
}
fn default_retries() -> u32 {
    3
}
fn default_timeout() -> u64 {
    30_000
}
fn default_resolve() -> u64 {
    5_000
}
fn default_balance() -> String {
    "roundrobin".into()
}
fn default_slots() -> usize {
    12
}
fn default_inter() -> u64 {
    2_000
}
fn default_rise() -> u32 {
    2
}
fn default_fall() -> u32 {
    3
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub listen: String,
    pub metrics_listen: String,
    #[serde(default = "default_health")]
    pub health_path: String,
    pub default_backend: String,
    #[serde(default)]
    pub route: Vec<Route>,
    pub backend: BTreeMap<String, Backend>,
    #[serde(default = "default_retries")]
    pub retries: u32,
    #[serde(default = "default_timeout")]
    pub timeout_ms: u64,
    #[serde(default = "default_resolve")]
    pub resolve_interval_ms: u64,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Route {
    pub prefix: String,
    pub backend: String,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Backend {
    /// A Service name; a headless one gives one address per ready pod.
    pub service: String,
    pub port: u16,
    #[serde(default = "default_balance")]
    pub balance: String,
    /// The health check's path.
    pub check: String,
    /// The health check's Host header (default: the service name).
    #[serde(default)]
    pub check_host: Option<String>,
    #[serde(default = "default_slots")]
    pub slots: usize,
    /// Insert-cookie affinity: the cookie's name.
    #[serde(default)]
    pub cookie: Option<String>,
    #[serde(default = "default_inter")]
    pub inter_ms: u64,
    #[serde(default = "default_rise")]
    pub rise: u32,
    #[serde(default = "default_fall")]
    pub fall: u32,
}

impl Config {
    /// Parses and validates; the error names what is wrong.
    pub fn parse(text: &str) -> Result<Config, String> {
        let cfg: Config = toml::from_str(text).map_err(|e| format!("not valid edge TOML: {e}"))?;
        cfg.validate()?;
        Ok(cfg)
    }

    pub fn validate(&self) -> Result<(), String> {
        for (what, addr) in [("listen", &self.listen), ("metrics_listen", &self.metrics_listen)] {
            let port = addr.rsplit(':').next().and_then(|p| p.parse::<u16>().ok()).ok_or_else(|| format!("{what} {addr:?} has no port"))?;
            if port < 1024 {
                return Err(format!("{what} port {port} is privileged; an arbitrary UID with no capabilities cannot bind it"));
            }
        }
        if self.listen == self.metrics_listen {
            return Err("listen and metrics_listen are the same address".into());
        }
        if !self.health_path.starts_with('/') {
            return Err(format!("health_path {:?} must start with /", self.health_path));
        }
        if self.backend.is_empty() {
            return Err("no backends".into());
        }
        if !self.backend.contains_key(&self.default_backend) {
            return Err(format!("default_backend {:?} is not a backend", self.default_backend));
        }
        for r in &self.route {
            if !r.prefix.starts_with('/') {
                return Err(format!("route prefix {:?} must start with /", r.prefix));
            }
            if !self.backend.contains_key(&r.backend) {
                return Err(format!("route {} names unknown backend {:?}", r.prefix, r.backend));
            }
        }
        for (name, b) in &self.backend {
            if b.service.is_empty() || b.port == 0 {
                return Err(format!("backend {name}: service and port are required"));
            }
            if b.balance != "roundrobin" && b.balance != "leastconn" {
                return Err(format!("backend {name}: balance {:?} (roundrobin or leastconn)", b.balance));
            }
            if !b.check.starts_with('/') {
                return Err(format!("backend {name}: check {:?} must start with /", b.check));
            }
            if b.slots == 0 || b.slots > 64 {
                return Err(format!("backend {name}: slots {} (1..=64)", b.slots));
            }
            if b.rise == 0 || b.fall == 0 || b.inter_ms < 100 {
                return Err(format!("backend {name}: rise and fall at least 1, inter_ms at least 100"));
            }
        }
        if self.timeout_ms < 100 || self.resolve_interval_ms < 500 {
            return Err("timeout_ms at least 100, resolve_interval_ms at least 500".into());
        }
        Ok(())
    }

    /// The backend for a path: the longest matching route prefix, else the default.
    pub fn backend_for(&self, path: &str) -> &str {
        self.route
            .iter()
            .filter(|r| path.starts_with(&r.prefix))
            .max_by_key(|r| r.prefix.len())
            .map(|r| r.backend.as_str())
            .unwrap_or(&self.default_backend)
    }
}

/// Validates the config file at `path`.
pub fn check_file(path: &Path) -> Result<Config, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    Config::parse(&text).map_err(|e| format!("{}: {e}", path.display()))
}

#[derive(Debug)]
struct Server {
    addr: SocketAddr,
    id: String,
    up: bool,
    checked: bool,
    ok: u32,
    fail: u32,
    active: Arc<AtomicU64>,
    requests: u64,
}

impl Server {
    fn new(addr: SocketAddr) -> Self {
        let id = format!("{:016x}", util::fnv1a(addr.to_string().as_bytes()));
        Server { addr, id, up: false, checked: false, ok: 0, fail: 0, active: Arc::new(AtomicU64::new(0)), requests: 0 }
    }
}

struct Pick {
    addr: SocketAddr,
    id: String,
    active: Arc<AtomicU64>,
}

#[derive(Default)]
struct Pool {
    servers: Mutex<Vec<Server>>,
    rr: AtomicUsize,
}

impl Pool {
    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<Server>> {
        self.servers.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Keeps the state of addresses still resolved, adds new ones (down until
    /// their first passing check), drops the rest.
    fn merge(&self, addrs: &[SocketAddr]) -> (usize, usize) {
        let mut servers = self.lock();
        let before = servers.len();
        servers.retain(|s| addrs.contains(&s.addr));
        let removed = before - servers.len();
        let mut added = 0;
        for a in addrs {
            if !servers.iter().any(|s| s.addr == *a) {
                servers.push(Server::new(*a));
                added += 1;
            }
        }
        servers.sort_by_key(|s| s.addr);
        (added, removed)
    }

    fn pick(&self, leastconn: bool, sticky: Option<&str>, exclude: &[SocketAddr]) -> Option<Pick> {
        let mut servers = self.lock();
        let candidates: Vec<usize> = (0..servers.len()).filter(|&i| servers[i].up && !exclude.contains(&servers[i].addr)).collect();
        if candidates.is_empty() {
            return None;
        }
        let sticky_hit = sticky.and_then(|id| candidates.iter().copied().find(|&i| servers[i].id == id));
        let start = self.rr.fetch_add(1, Ordering::Relaxed);
        let chosen = match sticky_hit {
            Some(i) => i,
            None if leastconn => {
                // Least active connections; ties broken round-robin.
                let n = candidates.len();
                (0..n).map(|k| candidates[(start + k) % n]).min_by_key(|&i| servers[i].active.load(Ordering::Relaxed)).unwrap_or(candidates[0])
            }
            None => candidates[start % candidates.len()],
        };
        let s = &mut servers[chosen];
        s.requests += 1;
        Some(Pick { addr: s.addr, id: s.id.clone(), active: Arc::clone(&s.active) })
    }

    fn up_count(&self) -> usize {
        self.lock().iter().filter(|s| s.up).count()
    }
}

pub struct Edge {
    cfg: RwLock<Arc<Config>>,
    checksum: AtomicU64,
    pools: RwLock<BTreeMap<String, Arc<Pool>>>,
    pod: String,
    seq: AtomicU64,
    reloads: AtomicU64,
    reload_failures: AtomicU64,
    counts: Mutex<BTreeMap<(String, u16), u64>>,
    live: PathBuf,
}

static HUP: AtomicBool = AtomicBool::new(false);

extern "C" fn on_hup(_: libc::c_int) {
    HUP.store(true, Ordering::SeqCst);
}

fn install_hup_handler() {
    let handler: extern "C" fn(libc::c_int) = on_hup;
    // SAFETY: the handler only stores to an atomic, which is async-signal-safe.
    unsafe {
        libc::signal(libc::SIGHUP, handler as libc::sighandler_t);
    }
}

fn resolve(service: &str, port: u16, slots: usize) -> std::io::Result<Vec<SocketAddr>> {
    let host = util::service_host(service);
    let mut addrs: Vec<SocketAddr> = (host.as_str(), port).to_socket_addrs()?.collect();
    addrs.sort();
    addrs.dedup();
    addrs.truncate(slots);
    Ok(addrs)
}

impl Edge {
    pub fn new(cfg: Config, live: PathBuf, checksum: u64) -> Self {
        let pools = cfg.backend.keys().map(|k| (k.clone(), Arc::new(Pool::default()))).collect();
        Edge {
            cfg: RwLock::new(Arc::new(cfg)),
            checksum: AtomicU64::new(checksum),
            pools: RwLock::new(pools),
            pod: util::pod_name(),
            seq: AtomicU64::new(0),
            reloads: AtomicU64::new(0),
            reload_failures: AtomicU64::new(0),
            counts: Mutex::new(BTreeMap::new()),
            live,
        }
    }

    pub fn config(&self) -> Arc<Config> {
        Arc::clone(&self.cfg.read().unwrap_or_else(|p| p.into_inner()))
    }

    fn pool(&self, name: &str) -> Option<Arc<Pool>> {
        self.pools.read().unwrap_or_else(|p| p.into_inner()).get(name).cloned()
    }

    /// Re-reads the live file; swaps the config in when it changed and validates.
    pub fn maybe_reload(&self, why: &str) {
        let text = match std::fs::read_to_string(&self.live) {
            Ok(t) => t,
            Err(e) => {
                eprintln!("[edge] cannot read {}: {e}", self.live.display());
                return;
            }
        };
        let sum = util::fnv1a(text.as_bytes());
        if sum == self.checksum.load(Ordering::SeqCst) {
            return;
        }
        match Config::parse(&text) {
            Ok(cfg) => {
                let old = self.config();
                if cfg.listen != old.listen || cfg.metrics_listen != old.metrics_listen {
                    eprintln!("[edge] listen addresses changed: that takes a restart; the rest is applied now");
                }
                {
                    let mut pools = self.pools.write().unwrap_or_else(|p| p.into_inner());
                    pools.retain(|k, _| cfg.backend.contains_key(k));
                    for k in cfg.backend.keys() {
                        pools.entry(k.clone()).or_default();
                    }
                }
                *self.cfg.write().unwrap_or_else(|p| p.into_inner()) = Arc::new(cfg);
                self.checksum.store(sum, Ordering::SeqCst);
                self.reloads.fetch_add(1, Ordering::Relaxed);
                println!("[edge] config reloaded ({why}), checksum {sum:016x}");
                self.resolve_all();
            }
            Err(e) => {
                // Remember the checksum so a bad file is reported once, not every poll.
                self.checksum.store(sum, Ordering::SeqCst);
                self.reload_failures.fetch_add(1, Ordering::Relaxed);
                eprintln!("[edge] REJECTED new config, keeping the running one: {e}");
            }
        }
    }

    fn resolve_all(&self) {
        let cfg = self.config();
        for (name, b) in &cfg.backend {
            let Some(pool) = self.pool(name) else { continue };
            match resolve(&b.service, b.port, b.slots) {
                Ok(addrs) => {
                    let (added, removed) = pool.merge(&addrs);
                    if added + removed > 0 {
                        println!("[edge] {name}: {} address(es) for {} (+{added} -{removed})", addrs.len(), b.service);
                    }
                }
                // NXDOMAIN while no pod is ready is normal (a headless Service
                // with no endpoints has no records): the pool empties.
                Err(e) => {
                    let (_, removed) = pool.merge(&[]);
                    if removed > 0 {
                        eprintln!("[edge] {name}: {} does not resolve ({e}); pool emptied", b.service);
                    }
                }
            }
        }
    }

    fn check_all(&self) {
        let cfg = self.config();
        for (name, b) in &cfg.backend {
            let Some(pool) = self.pool(name) else { continue };
            let addrs: Vec<SocketAddr> = pool.lock().iter().map(|s| s.addr).collect();
            let host = b.check_host.clone().unwrap_or_else(|| b.service.clone());
            let results: Vec<(SocketAddr, bool)> = std::thread::scope(|scope| {
                let handles: Vec<_> = addrs
                    .iter()
                    .map(|a| {
                        let (a, host, path) = (*a, host.clone(), b.check.clone());
                        scope.spawn(move || {
                            let mut req = Request::new("GET", &path);
                            req.set_header("User-Agent", "storefront-edge-check");
                            let ok = http::send(a, &host, &req, Duration::from_secs(2)).is_ok_and(|r| r.status == 200);
                            (a, ok)
                        })
                    })
                    .collect();
                handles.into_iter().filter_map(|h| h.join().ok()).collect()
            });
            let mut servers = pool.lock();
            for (addr, ok) in results {
                let Some(s) = servers.iter_mut().find(|s| s.addr == addr) else { continue };
                if ok {
                    s.ok += 1;
                    s.fail = 0;
                    // A never-checked server comes up on its first pass, so a
                    // new pod takes traffic as soon as it answers.
                    if !s.up && (!s.checked || s.ok >= b.rise) {
                        s.up = true;
                        println!("[edge] {name}/{addr} is UP");
                    }
                } else {
                    s.fail += 1;
                    s.ok = 0;
                    if s.up && s.fail >= b.fall {
                        s.up = false;
                        println!("[edge] {name}/{addr} is DOWN");
                    }
                }
                s.checked = true;
            }
        }
    }

    fn count(&self, backend: &str, status: u16) {
        *self.counts.lock().unwrap_or_else(|p| p.into_inner()).entry((backend.to_string(), status)).or_insert(0) += 1;
    }

    fn request_id(&self) -> String {
        let n = self.seq.fetch_add(1, Ordering::Relaxed);
        format!("{:08x}-{:x}-{n:x}", util::fnv1a(self.pod.as_bytes()) as u32, util::unix_now())
    }

    pub fn handle(&self, req: Request) -> Response {
        let cfg = self.config();
        if req.path() == cfg.health_path {
            return Response::text(200, "ok\n").with_header("X-Served-By", &self.pod);
        }
        let backend_name = cfg.backend_for(req.path()).to_string();
        let resp = self.forward(&cfg, &backend_name, req);
        self.count(&backend_name, resp.status);
        resp.with_header("X-Served-By", &self.pod)
    }

    fn forward(&self, cfg: &Config, name: &str, req: Request) -> Response {
        let (Some(b), Some(pool)) = (cfg.backend.get(name), self.pool(name)) else {
            return Response::text(503, "no such backend\n");
        };
        let mut out = req.clone();
        let rid = match req.header("x-request-id") {
            Some(r) => r.to_string(),
            None => self.request_id(),
        };
        out.set_header("X-Request-Id", &rid);
        let peer = req.peer.map(|p| p.ip().to_string()).unwrap_or_default();
        let xff = match req.header("x-forwarded-for") {
            Some(prev) => format!("{prev}, {peer}"),
            None => peer,
        };
        out.set_header("X-Forwarded-For", &xff);
        let host = req.header("host").unwrap_or(b.service.as_str()).to_string();
        let sticky = b.cookie.as_ref().and_then(|c| cookie_value(req.header("cookie").unwrap_or(""), c));
        let retriable_any = req.method == "GET" || req.method == "HEAD";
        let timeout = Duration::from_millis(cfg.timeout_ms);
        let mut tried = Vec::new();
        for _ in 0..=cfg.retries {
            let Some(pick) = pool.pick(b.balance == "leastconn", sticky.as_deref(), &tried) else { break };
            tried.push(pick.addr);
            pick.active.fetch_add(1, Ordering::Relaxed);
            let result = http::send(pick.addr, &host, &out, timeout);
            pick.active.fetch_sub(1, Ordering::Relaxed);
            match result {
                Ok(mut resp) => {
                    resp.headers.retain(|(k, _)| !http::is_hop_by_hop(k));
                    resp.headers.push(("X-Request-Id".into(), rid));
                    if let Some(c) = &b.cookie {
                        if sticky.as_deref() != Some(pick.id.as_str()) {
                            resp.headers.push(("Set-Cookie".into(), format!("{c}={}; Path=/; HttpOnly", pick.id)));
                        }
                    }
                    return resp;
                }
                Err(e) => {
                    eprintln!("[edge] {name}/{} {} {}: {e}", pick.addr, req.method, req.target);
                    let refused = e.kind() == std::io::ErrorKind::ConnectionRefused;
                    if !refused && !retriable_any {
                        return Response::text(502, "bad gateway\n").with_header("X-Request-Id", &rid);
                    }
                }
            }
        }
        if tried.is_empty() {
            Response::text(503, "no server available\n").with_header("X-Request-Id", &rid)
        } else {
            Response::text(502, "bad gateway\n").with_header("X-Request-Id", &rid)
        }
    }

    pub fn metrics(&self) -> String {
        let mut out = String::new();
        out.push_str("# HELP storefront_edge_backend_active_servers Servers up in each backend.\n# TYPE storefront_edge_backend_active_servers gauge\n");
        let pools: Vec<(String, Arc<Pool>)> =
            self.pools.read().unwrap_or_else(|p| p.into_inner()).iter().map(|(k, v)| (k.clone(), Arc::clone(v))).collect();
        for (name, pool) in &pools {
            out.push_str(&format!("storefront_edge_backend_active_servers{{backend=\"{name}\"}} {}\n", pool.up_count()));
        }
        out.push_str("# HELP storefront_edge_server_up Whether each resolved server passes its checks.\n# TYPE storefront_edge_server_up gauge\n");
        for (name, pool) in &pools {
            for s in pool.lock().iter() {
                out.push_str(&format!("storefront_edge_server_up{{backend=\"{name}\",server=\"{}\"}} {}\n", s.addr, u8::from(s.up)));
            }
        }
        out.push_str("# HELP storefront_edge_requests_total Requests by backend and status.\n# TYPE storefront_edge_requests_total counter\n");
        for ((backend, status), n) in self.counts.lock().unwrap_or_else(|p| p.into_inner()).iter() {
            out.push_str(&format!("storefront_edge_requests_total{{backend=\"{backend}\",code=\"{status}\"}} {n}\n"));
        }
        out.push_str(&format!(
            "# TYPE storefront_edge_reloads_total counter\nstorefront_edge_reloads_total {}\n# TYPE storefront_edge_reload_failures_total counter\nstorefront_edge_reload_failures_total {}\n# TYPE storefront_edge_config_info gauge\nstorefront_edge_config_info{{checksum=\"{:016x}\",pod=\"{}\"}} 1\n",
            self.reloads.load(Ordering::Relaxed),
            self.reload_failures.load(Ordering::Relaxed),
            self.checksum.load(Ordering::SeqCst),
            util::label(&self.pod)
        ));
        out
    }

    pub fn stats(&self) -> serde_json::Value {
        let pools = self.pools.read().unwrap_or_else(|p| p.into_inner());
        let mut backends = serde_json::Map::new();
        for (name, pool) in pools.iter() {
            let servers: Vec<_> = pool
                .lock()
                .iter()
                .map(|s| serde_json::json!({"server": s.addr.to_string(), "up": s.up, "active": s.active.load(Ordering::Relaxed), "requests": s.requests}))
                .collect();
            backends.insert(name.clone(), serde_json::Value::Array(servers));
        }
        serde_json::json!({"pod": self.pod, "checksum": format!("{:016x}", self.checksum.load(Ordering::SeqCst)), "backends": backends})
    }
}

/// The value of cookie `name` in a `Cookie:` header.
pub fn cookie_value(header: &str, name: &str) -> Option<String> {
    header.split(';').filter_map(|kv| kv.trim().split_once('=')).find(|(k, _)| *k == name).map(|(_, v)| v.to_string())
}

pub fn run() -> std::io::Result<()> {
    let live = PathBuf::from(util::env_or("EDGE_CONFIG", "/etc/edge/live/edge.toml"));
    let text = std::fs::read_to_string(&live)?;
    let cfg = Config::parse(&text).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    let checksum = util::fnv1a(text.as_bytes());
    let (listen, metrics_listen) = (cfg.listen.clone(), cfg.metrics_listen.clone());
    let edge = Arc::new(Edge::new(cfg, live.clone(), checksum));
    install_hup_handler();
    edge.resolve_all();
    edge.check_all();

    // Resolver: re-reads the headless Services' records (HAProxy's `hold valid`).
    {
        let edge = Arc::clone(&edge);
        std::thread::spawn(move || loop {
            std::thread::sleep(Duration::from_millis(edge.config().resolve_interval_ms));
            edge.resolve_all();
        });
    }
    // Checker: per-server health checks.
    {
        let edge = Arc::clone(&edge);
        std::thread::spawn(move || loop {
            let inter = edge.config().backend.values().map(|b| b.inter_ms).min().unwrap_or(2_000);
            std::thread::sleep(Duration::from_millis(inter));
            edge.check_all();
        });
    }
    // Reload: SIGHUP from the reloader, or the live file changing under us.
    {
        let edge = Arc::clone(&edge);
        std::thread::spawn(move || {
            let mut ticks = 0u32;
            loop {
                std::thread::sleep(Duration::from_millis(250));
                ticks += 1;
                if HUP.swap(false, Ordering::SeqCst) {
                    edge.maybe_reload("SIGHUP");
                } else if ticks >= 8 {
                    ticks = 0;
                    edge.maybe_reload("file changed");
                }
            }
        });
    }
    {
        let edge = Arc::clone(&edge);
        http::spawn_server(&metrics_listen, move |req| match req.path() {
            "/metrics" => Response::new(200, "text/plain; version=0.0.4", edge.metrics().into_bytes()),
            "/stats" => Response::json(200, &edge.stats()),
            _ => Response::text(404, "not found\n"),
        })?;
    }
    println!("[edge] listening on {listen}, metrics on {metrics_listen}, config {}", live.display());
    http::serve(&listen, move |req| edge.handle(req))
}

#[cfg(test)]
mod tests {
    use super::*;

    pub const SAMPLE: &str = r#"
listen = ":8080"
metrics_listen = ":8404"
default_backend = "web"

[[route]]
prefix = "/api/"
backend = "api"

[backend.web]
service = "web-headless"
port = 8080
check = "/healthz"
cookie = "SRVID"

[backend.api]
service = "api-headless"
port = 8081
balance = "leastconn"
check = "/api/readyz"
"#;

    #[test]
    fn parses_and_routes() {
        let c = Config::parse(SAMPLE).unwrap();
        assert_eq!(c.backend_for("/api/products"), "api");
        assert_eq!(c.backend_for("/"), "web");
        assert_eq!(c.backend_for("/apix"), "web");
        assert_eq!(c.backend["web"].slots, 12);
    }

    #[test]
    fn refuses_bad_configs() {
        assert!(Config::parse(&SAMPLE.replace(":8080", ":80")).unwrap_err().contains("privileged"));
        assert!(Config::parse(&SAMPLE.replace("leastconn", "random")).is_err());
        assert!(Config::parse(&SAMPLE.replace("backend = \"api\"", "backend = \"nope\"")).is_err());
        assert!(Config::parse(&format!("{SAMPLE}\nbogus = 1\n")).is_err());
        assert!(Config::parse("not toml [").is_err());
    }

    #[test]
    fn pools_balance_and_stick() {
        let pool = Pool::default();
        let a: SocketAddr = "10.0.0.1:8080".parse().unwrap();
        let b: SocketAddr = "10.0.0.2:8080".parse().unwrap();
        assert_eq!(pool.merge(&[a, b]), (2, 0));
        assert!(pool.pick(false, None, &[]).is_none(), "servers start down");
        for s in pool.lock().iter_mut() {
            s.up = true;
        }
        let first = pool.pick(false, None, &[]).unwrap().addr;
        let second = pool.pick(false, None, &[]).unwrap().addr;
        assert_ne!(first, second, "round robin alternates");
        let id_b = Server::new(b).id;
        for _ in 0..4 {
            assert_eq!(pool.pick(false, Some(&id_b), &[]).unwrap().addr, b, "the cookie sticks");
        }
        assert_eq!(pool.pick(true, None, &[a]).unwrap().addr, b, "excluded servers are skipped");
        // leastconn spreads across idle servers too.
        let picks: std::collections::BTreeSet<_> = (0..4).map(|_| pool.pick(true, None, &[]).unwrap().addr).collect();
        assert_eq!(picks.len(), 2);
        assert_eq!(pool.merge(&[b]), (0, 1));
        assert_eq!(pool.up_count(), 1);
    }

    #[test]
    fn cookies_parse() {
        assert_eq!(cookie_value("a=1; SRVID=abc; b=2", "SRVID").as_deref(), Some("abc"));
        assert_eq!(cookie_value("", "SRVID"), None);
    }
}
