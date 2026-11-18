//! The database tier (`store` role), standing where the original runs
//! PostgreSQL 15: the catalog and the orders, kept on the `data` PVC.
//!
//! An HTTP/JSON API on `STORE_LISTEN` (default :5432, the port named
//! `postgres` in the manifests), every `/v1/` call authenticated with
//! `Authorization: Bearer <database-password>` from the `storefront-db`
//! Secret — the same Secret key the original's postgres reads.
//!
//! ```text
//! GET  /healthz                 liveness
//! GET  /readyz                  ready once the data directory is seeded and writable
//! GET  /v1/products             the catalog
//! GET  /v1/products/<sku>
//! POST /v1/orders               {"sku": "...", "qty": n} -> the order, with its id
//! GET  /v1/orders/<id>
//! GET  /v1/dump                 everything, for the backup sidecar
//! ```
//!
//! The catalog is seeded once, on an empty volume, from the ConfigMap's
//! `seed.json` (as the original's `seed.sql` runs from
//! /docker-entrypoint-initdb.d). Every write is a file written beside its
//! target, synced and renamed into place, so a pod killed mid-write leaves the
//! old version or the new one, never half of one.
//!
//! Counters are on loopback only (`STATS_LISTEN`, default 127.0.0.1:5433,
//! `/stats`) for the `exporter` sidecar, as postgres_exporter reads postgres
//! over loopback.

use crate::http::{self, Request, Response};
use crate::util;
use serde_json::{json, Value};
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

pub struct Store {
    dir: PathBuf,
    token: Option<String>,
    write_lock: Mutex<()>,
    started: Instant,
    boots: u64,
    requests: AtomicU64,
    orders_created: AtomicU64,
    auth_failures: AtomicU64,
}

/// Writes `data` to `path` atomically: a sibling temp file, fsync, rename.
pub fn write_atomic(path: &Path, data: &[u8]) -> io::Result<()> {
    let tmp = path.with_extension("part");
    {
        let mut f = fs::File::create(&tmp)?;
        f.write_all(data)?;
        f.sync_all()?;
    }
    fs::rename(&tmp, path)?;
    if let Some(dir) = path.parent() {
        if let Ok(d) = fs::File::open(dir) {
            let _ = d.sync_all();
        }
    }
    Ok(())
}

fn read_json(path: &Path) -> io::Result<Value> {
    let raw = fs::read(path)?;
    serde_json::from_slice(&raw).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("{}: {e}", path.display())))
}

/// Checks a seed document: `{"products": [{"sku","name","price_cents","stock"}, …]}`.
pub fn check_seed(seed: &Value) -> Result<usize, String> {
    let products = seed.get("products").and_then(Value::as_array).ok_or("seed has no products array")?;
    if products.is_empty() {
        return Err("seed has no products".into());
    }
    let mut skus = std::collections::BTreeSet::new();
    for p in products {
        let sku = p.get("sku").and_then(Value::as_str).ok_or("a product has no sku")?;
        if !skus.insert(sku.to_string()) {
            return Err(format!("sku {sku} appears twice"));
        }
        p.get("name").and_then(Value::as_str).ok_or_else(|| format!("{sku}: no name"))?;
        p.get("price_cents").and_then(Value::as_u64).ok_or_else(|| format!("{sku}: price_cents must be a non-negative integer"))?;
        p.get("stock").and_then(Value::as_u64).ok_or_else(|| format!("{sku}: stock must be a non-negative integer"))?;
    }
    Ok(products.len())
}

impl Store {
    /// Opens (and on first use seeds) the data directory.
    pub fn open(dir: &Path, seed_file: Option<&Path>, token: Option<String>) -> io::Result<Self> {
        fs::create_dir_all(dir.join("orders"))?;
        let products = dir.join("products.json");
        if !products.exists() {
            let seed = match seed_file {
                Some(f) => read_json(f)?,
                None => return Err(io::Error::new(io::ErrorKind::NotFound, "empty data directory and no seed")),
            };
            check_seed(&seed).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
            write_atomic(&products, serde_json::to_vec_pretty(&seed["products"])?.as_slice())?;
            write_atomic(&dir.join("initialized_at"), util::rfc3339(util::unix_now()).as_bytes())?;
            println!("[store] seeded {} from {}", dir.display(), seed_file.map(|p| p.display().to_string()).unwrap_or_default());
        }
        let boots = fs::read_to_string(dir.join("boots")).ok().and_then(|s| s.trim().parse::<u64>().ok()).unwrap_or(0) + 1;
        write_atomic(&dir.join("boots"), boots.to_string().as_bytes())?;
        Ok(Store {
            dir: dir.to_path_buf(),
            token,
            write_lock: Mutex::new(()),
            started: Instant::now(),
            boots,
            requests: AtomicU64::new(0),
            orders_created: AtomicU64::new(0),
            auth_failures: AtomicU64::new(0),
        })
    }

    pub fn products(&self) -> io::Result<Value> {
        read_json(&self.dir.join("products.json"))
    }

    fn order_path(&self, id: u64) -> PathBuf {
        self.dir.join("orders").join(format!("{id:010}.json"))
    }

    pub fn order(&self, id: u64) -> io::Result<Option<Value>> {
        match read_json(&self.order_path(id)) {
            Ok(v) => Ok(Some(v)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    pub fn orders(&self) -> io::Result<Vec<Value>> {
        let mut names: Vec<_> = fs::read_dir(self.dir.join("orders"))?
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".json"))
            .collect();
        names.sort();
        names.iter().map(|n| read_json(&self.dir.join("orders").join(n))).collect()
    }

    /// Creates an order for a known sku; `Err(msg)` for a client error.
    pub fn create_order(&self, sku: &str, qty: u64) -> io::Result<Result<Value, String>> {
        if qty == 0 || qty > 1000 {
            return Ok(Err("qty must be between 1 and 1000".into()));
        }
        let products = self.products()?;
        let Some(product) = products.as_array().and_then(|ps| ps.iter().find(|p| p["sku"] == sku)) else {
            return Ok(Err(format!("unknown sku {sku}")));
        };
        let _guard = self.write_lock.lock().unwrap_or_else(|p| p.into_inner());
        let seq = self.dir.join("next_order");
        let id = fs::read_to_string(&seq).ok().and_then(|s| s.trim().parse::<u64>().ok()).unwrap_or(1);
        let order = json!({
            "id": id,
            "sku": sku,
            "name": product["name"],
            "qty": qty,
            "total_cents": product["price_cents"].as_u64().unwrap_or(0) * qty,
            "created_at": util::rfc3339(util::unix_now()),
        });
        write_atomic(&self.order_path(id), serde_json::to_vec_pretty(&order)?.as_slice())?;
        write_atomic(&seq, (id + 1).to_string().as_bytes())?;
        self.orders_created.fetch_add(1, Ordering::Relaxed);
        Ok(Ok(order))
    }

    pub fn dump(&self) -> io::Result<Value> {
        Ok(json!({
            "products": self.products()?,
            "orders": self.orders()?,
            "initialized_at": fs::read_to_string(self.dir.join("initialized_at")).unwrap_or_default(),
            "dumped_at": util::rfc3339(util::unix_now()),
        }))
    }

    fn ready(&self) -> bool {
        let probe = self.dir.join(".ready-probe");
        self.dir.join("products.json").exists() && fs::write(&probe, b"ok").is_ok() && fs::remove_file(&probe).is_ok()
    }

    fn authorized(&self, req: &Request) -> bool {
        match &self.token {
            None => true,
            Some(t) => req.header("authorization").and_then(|h| h.strip_prefix("Bearer ")).is_some_and(|given| given == t),
        }
    }

    pub fn stats_text(&self) -> String {
        let orders = fs::read_dir(self.dir.join("orders")).map(|d| d.count()).unwrap_or(0);
        format!(
            "up 1\nuptime_seconds {}\nboots_total {}\nrequests_total {}\norders_created_total {}\norders {}\nauth_failures_total {}\n",
            self.started.elapsed().as_secs(),
            self.boots,
            self.requests.load(Ordering::Relaxed),
            self.orders_created.load(Ordering::Relaxed),
            orders,
            self.auth_failures.load(Ordering::Relaxed),
        )
    }

    pub fn handle(&self, req: &Request) -> Response {
        self.requests.fetch_add(1, Ordering::Relaxed);
        let path = req.path().trim_end_matches('/');
        match (req.method.as_str(), path) {
            ("GET", "/healthz") => return Response::text(200, "ok\n"),
            ("GET", "/readyz") => {
                return if self.ready() { Response::text(200, "ready\n") } else { Response::text(503, "not ready\n") };
            }
            _ => {}
        }
        if !path.starts_with("/v1/") {
            return Response::json(404, &json!({"error": "not found", "path": path}));
        }
        if !self.authorized(req) {
            self.auth_failures.fetch_add(1, Ordering::Relaxed);
            return Response::json(401, &json!({"error": "unauthorized"}));
        }
        let result = match (req.method.as_str(), path) {
            ("GET", "/v1/products") => self.products().map(|p| Response::json(200, &json!({"products": p}))),
            ("GET", p) if p.starts_with("/v1/products/") => {
                let sku = &p["/v1/products/".len()..];
                self.products().map(|ps| match ps.as_array().and_then(|a| a.iter().find(|x| x["sku"] == sku)) {
                    Some(prod) => Response::json(200, prod),
                    None => Response::json(404, &json!({"error": "no such product", "sku": sku})),
                })
            }
            ("POST", "/v1/orders") => {
                let body: Value = match serde_json::from_slice(&req.body) {
                    Ok(v) => v,
                    Err(e) => return Response::json(400, &json!({"error": format!("bad json: {e}")})),
                };
                let sku = body["sku"].as_str().unwrap_or("");
                let qty = body["qty"].as_u64().unwrap_or(1);
                self.create_order(sku, qty).map(|r| match r {
                    Ok(order) => Response::json(201, &order),
                    Err(msg) => Response::json(400, &json!({"error": msg})),
                })
            }
            ("GET", p) if p.starts_with("/v1/orders/") => match p["/v1/orders/".len()..].parse::<u64>() {
                Ok(id) => self.order(id).map(|o| match o {
                    Some(o) => Response::json(200, &o),
                    None => Response::json(404, &json!({"error": "no such order", "id": id})),
                }),
                Err(_) => Ok(Response::json(400, &json!({"error": "order ids are numbers"}))),
            },
            ("GET", "/v1/dump") => self.dump().map(|d| Response::json(200, &d)),
            _ => Ok(Response::json(404, &json!({"error": "not found", "path": path}))),
        };
        result.unwrap_or_else(|e| {
            eprintln!("[store] {} {}: {e}", req.method, path);
            Response::json(500, &json!({"error": e.to_string()}))
        })
    }
}

pub fn run() -> io::Result<()> {
    let dir = PathBuf::from(util::env_or("STORE_DATA", "/var/lib/store/data"));
    let seed = util::env_opt("STORE_SEED").map(PathBuf::from);
    let token = util::env_opt("STORE_PASSWORD");
    if token.is_none() {
        eprintln!("[store] WARNING: STORE_PASSWORD is empty; the API is unauthenticated");
    }
    let store = Arc::new(Store::open(&dir, seed.as_deref(), token)?);
    {
        let store = Arc::clone(&store);
        http::spawn_server(&util::env_or("STATS_LISTEN", "127.0.0.1:5433"), move |req| match req.path() {
            "/stats" => Response::text(200, &store.stats_text()),
            _ => Response::text(404, "not found\n"),
        })?;
    }
    let listen = util::env_or("STORE_LISTEN", ":5432");
    println!("[store] listening on {listen}, data {} (boot {})", dir.display(), store.boots);
    http::serve(&listen, move |req| store.handle(&req))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("storefront-store-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn seed(dir: &Path) -> PathBuf {
        let f = dir.join("seed.json");
        fs::write(&f, br#"{"products":[{"sku":"ANV-001","name":"Anvil, 50kg","price_cents":12900,"stock":7}]}"#).unwrap();
        f
    }

    fn authed(method: &str, target: &str, body: &[u8]) -> Request {
        let mut r = Request::new(method, target);
        r.set_header("Authorization", "Bearer pw");
        r.body = body.to_vec();
        r
    }

    #[test]
    fn orders_survive_a_reopen() {
        let base = tmpdir("reopen");
        let data = base.join("data");
        let s = Store::open(&data, Some(&seed(&base)), Some("pw".into())).unwrap();
        let r = s.handle(&authed("POST", "/v1/orders", br#"{"sku":"ANV-001","qty":2}"#));
        assert_eq!(r.status, 201, "{}", r.body_str());
        assert!(r.body_str().contains("\"total_cents\": 25800"));
        drop(s);
        // A restart: no seed this time, the data is already there.
        let s = Store::open(&data, None, Some("pw".into())).unwrap();
        assert_eq!(s.boots, 2);
        let r = s.handle(&authed("GET", "/v1/orders/1", b""));
        assert_eq!(r.status, 200);
        assert!(r.body_str().contains("ANV-001"));
        assert_eq!(s.handle(&authed("GET", "/v1/orders/2", b"")).status, 404);
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn refuses_without_the_token_and_bad_orders() {
        let base = tmpdir("auth");
        let s = Store::open(&base.join("data"), Some(&seed(&base)), Some("pw".into())).unwrap();
        assert_eq!(s.handle(&Request::new("GET", "/v1/products")).status, 401);
        assert_eq!(s.handle(&Request::new("GET", "/healthz")).status, 200);
        assert_eq!(s.handle(&Request::new("GET", "/readyz")).status, 200);
        assert_eq!(s.handle(&authed("POST", "/v1/orders", br#"{"sku":"NOPE","qty":1}"#)).status, 400);
        assert_eq!(s.handle(&authed("POST", "/v1/orders", b"not json")).status, 400);
        assert_eq!(s.handle(&authed("GET", "/v1/products/ANV-001", b"")).status, 200);
        let dump = s.handle(&authed("GET", "/v1/dump", b""));
        assert!(dump.body_str().contains("initialized_at"));
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn seeds_are_checked() {
        assert!(check_seed(&json!({"products": []})).is_err());
        assert!(check_seed(&json!({"products": [{"sku": "A", "name": "a", "price_cents": 1, "stock": 1}, {"sku": "A", "name": "b", "price_cents": 1, "stock": 1}]})).is_err());
        assert_eq!(check_seed(&json!({"products": [{"sku": "A", "name": "a", "price_cents": 1, "stock": 0}]})), Ok(1));
    }
}
