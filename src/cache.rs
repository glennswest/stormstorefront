//! The cache tier (`cache` role): a small RESP key-value server standing where
//! the original runs Redis 7. Password-protected, deliberately non-persistent.
//!
//! Commands: AUTH PING ECHO GET SET (EX/PX) SETEX DEL EXISTS EXPIRE TTL
//! INCR DBSIZE FLUSHALL INFO SELECT QUIT COMMAND. Enough for the API and for a
//! person with any Redis client.
//!
//! Its counters are served on loopback only (`STATS_LISTEN`, default
//! 127.0.0.1:6380, `/stats`), which the `exporter` sidecar turns into
//! Prometheus text, as redis_exporter scrapes Redis over loopback.

use crate::http::{self, Response};
use crate::resp::{read_value, Value};
use crate::util;
use std::collections::HashMap;
use std::io::{BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

type Entry = (Vec<u8>, Option<Instant>);

#[derive(Default)]
pub struct Stats {
    pub connections: AtomicU64,
    pub commands: AtomicU64,
    pub hits: AtomicU64,
    pub misses: AtomicU64,
    pub auth_failures: AtomicU64,
}

pub struct Store {
    map: Mutex<HashMap<Vec<u8>, Entry>>,
    password: Option<String>,
    pub stats: Stats,
    started: Instant,
}

fn arg_str(a: &[u8]) -> String {
    String::from_utf8_lossy(a).to_ascii_uppercase()
}

fn err(msg: &str) -> Value {
    Value::Error(msg.to_string())
}

impl Store {
    pub fn new(password: Option<String>) -> Self {
        Store { map: Mutex::new(HashMap::new()), password, stats: Stats::default(), started: Instant::now() }
    }

    fn live<'a>(map: &'a mut HashMap<Vec<u8>, Entry>, key: &[u8]) -> Option<&'a mut Entry> {
        let expired = map.get(key).is_some_and(|(_, exp)| exp.is_some_and(|e| e <= Instant::now()));
        if expired {
            map.remove(key);
        }
        map.get_mut(key)
    }

    fn keys(&self) -> usize {
        let mut map = self.map.lock().unwrap_or_else(|p| p.into_inner());
        let now = Instant::now();
        map.retain(|_, (_, exp)| !exp.is_some_and(|e| e <= now));
        map.len()
    }

    /// Executes one command for a connection; `authed` is the connection's
    /// state. Returns the reply and whether to close afterwards.
    pub fn execute(&self, args: &[Vec<u8>], authed: &mut bool) -> (Value, bool) {
        let Some(first) = args.first() else {
            return (err("ERR empty command"), false);
        };
        self.stats.commands.fetch_add(1, Ordering::Relaxed);
        let cmd = arg_str(first);
        if cmd == "QUIT" {
            return (Value::ok(), true);
        }
        if cmd == "AUTH" {
            let given = args.last().map(|a| String::from_utf8_lossy(a).into_owned()).unwrap_or_default();
            return match &self.password {
                None => (err("ERR AUTH <password> called without any password configured for the default user"), false),
                Some(pw) if args.len() >= 2 && *pw == given => {
                    *authed = true;
                    (Value::ok(), false)
                }
                Some(_) => {
                    self.stats.auth_failures.fetch_add(1, Ordering::Relaxed);
                    (err("WRONGPASS invalid username-password pair or user is disabled."), false)
                }
            };
        }
        if self.password.is_some() && !*authed && cmd != "PING" {
            return (err("NOAUTH Authentication required."), false);
        }
        (self.run(&cmd, &args[1..]), false)
    }

    fn run(&self, cmd: &str, a: &[Vec<u8>]) -> Value {
        let mut map = self.map.lock().unwrap_or_else(|p| p.into_inner());
        let arity = |n: usize| a.len() >= n;
        match cmd {
            "PING" => match a.first() {
                Some(m) => Value::bulk(m),
                None => Value::Simple("PONG".into()),
            },
            "ECHO" if arity(1) => Value::bulk(&a[0]),
            "SELECT" | "CLIENT" | "CONFIG" => Value::ok(),
            "COMMAND" => Value::Array(Some(Vec::new())),
            "GET" if arity(1) => match Self::live(&mut map, &a[0]) {
                Some((v, _)) => {
                    self.stats.hits.fetch_add(1, Ordering::Relaxed);
                    Value::bulk(v)
                }
                None => {
                    self.stats.misses.fetch_add(1, Ordering::Relaxed);
                    Value::Bulk(None)
                }
            },
            "SET" if arity(2) => {
                let mut exp = None;
                let mut i = 2;
                while i < a.len() {
                    let opt = arg_str(&a[i]);
                    let n: Option<u64> = a.get(i + 1).and_then(|v| String::from_utf8_lossy(v).parse().ok());
                    match (opt.as_str(), n) {
                        ("EX", Some(s)) if s > 0 => exp = Some(Instant::now() + Duration::from_secs(s)),
                        ("PX", Some(ms)) if ms > 0 => exp = Some(Instant::now() + Duration::from_millis(ms)),
                        _ => return err("ERR syntax error"),
                    }
                    i += 2;
                }
                map.insert(a[0].clone(), (a[1].clone(), exp));
                Value::ok()
            }
            "SETEX" if arity(3) => match String::from_utf8_lossy(&a[1]).parse::<u64>() {
                Ok(s) if s > 0 => {
                    map.insert(a[0].clone(), (a[2].clone(), Some(Instant::now() + Duration::from_secs(s))));
                    Value::ok()
                }
                _ => err("ERR invalid expire time in 'setex' command"),
            },
            "DEL" if arity(1) => Value::Int(a.iter().filter(|k| map.remove(k.as_slice()).is_some()).count() as i64),
            "EXISTS" if arity(1) => {
                let mut n = 0;
                for k in a {
                    if Self::live(&mut map, k).is_some() {
                        n += 1;
                    }
                }
                Value::Int(n)
            }
            "EXPIRE" if arity(2) => match (String::from_utf8_lossy(&a[1]).parse::<u64>(), Self::live(&mut map, &a[0])) {
                (Ok(s), Some(entry)) => {
                    entry.1 = Some(Instant::now() + Duration::from_secs(s));
                    Value::Int(1)
                }
                (Ok(_), None) => Value::Int(0),
                _ => err("ERR value is not an integer or out of range"),
            },
            "TTL" if arity(1) => match Self::live(&mut map, &a[0]) {
                None => Value::Int(-2),
                Some((_, None)) => Value::Int(-1),
                Some((_, Some(e))) => Value::Int(e.saturating_duration_since(Instant::now()).as_secs() as i64),
            },
            "INCR" if arity(1) => {
                let cur = Self::live(&mut map, &a[0]).map(|(v, e)| (String::from_utf8_lossy(v).parse::<i64>(), *e));
                match cur {
                    None => {
                        map.insert(a[0].clone(), (b"1".to_vec(), None));
                        Value::Int(1)
                    }
                    Some((Ok(n), exp)) => {
                        map.insert(a[0].clone(), ((n + 1).to_string().into_bytes(), exp));
                        Value::Int(n + 1)
                    }
                    Some((Err(_), _)) => err("ERR value is not an integer or out of range"),
                }
            }
            "DBSIZE" => {
                drop(map);
                Value::Int(self.keys() as i64)
            }
            "FLUSHALL" | "FLUSHDB" => {
                map.clear();
                Value::ok()
            }
            "INFO" => {
                drop(map);
                Value::bulk(self.info().as_bytes())
            }
            _ => err(&format!("ERR unknown command or wrong number of arguments for '{}'", cmd.to_ascii_lowercase())),
        }
    }

    fn info(&self) -> String {
        format!(
            "# Server\r\nredis_version:7.0.0-storefront\r\nuptime_in_seconds:{}\r\n# Stats\r\ntotal_connections_received:{}\r\ntotal_commands_processed:{}\r\nkeyspace_hits:{}\r\nkeyspace_misses:{}\r\n# Keyspace\r\ndb0:keys={}\r\n",
            self.started.elapsed().as_secs(),
            self.stats.connections.load(Ordering::Relaxed),
            self.stats.commands.load(Ordering::Relaxed),
            self.stats.hits.load(Ordering::Relaxed),
            self.stats.misses.load(Ordering::Relaxed),
            self.keys()
        )
    }

    /// `name value` lines for the exporter.
    pub fn stats_text(&self) -> String {
        format!(
            "up 1\nuptime_seconds {}\nconnections_total {}\ncommands_total {}\nkeyspace_hits_total {}\nkeyspace_misses_total {}\nauth_failures_total {}\nkeys {}\n",
            self.started.elapsed().as_secs(),
            self.stats.connections.load(Ordering::Relaxed),
            self.stats.commands.load(Ordering::Relaxed),
            self.stats.hits.load(Ordering::Relaxed),
            self.stats.misses.load(Ordering::Relaxed),
            self.stats.auth_failures.load(Ordering::Relaxed),
            self.keys()
        )
    }
}

fn to_args(v: Value) -> Option<Vec<Vec<u8>>> {
    match v {
        Value::Array(Some(items)) => items
            .into_iter()
            .map(|i| match i {
                Value::Bulk(Some(b)) => Some(b),
                Value::Simple(s) => Some(s.into_bytes()),
                Value::Int(n) => Some(n.to_string().into_bytes()),
                _ => None,
            })
            .collect(),
        _ => None,
    }
}

fn handle(stream: TcpStream, store: &Store) -> std::io::Result<()> {
    store.stats.connections.fetch_add(1, Ordering::Relaxed);
    let mut w = stream.try_clone()?;
    let mut r = BufReader::new(stream);
    let mut authed = false;
    while let Some(v) = read_value(&mut r)? {
        let (reply, close) = match to_args(v) {
            Some(args) if args.is_empty() => continue,
            Some(args) => store.execute(&args, &mut authed),
            None => (err("ERR Protocol error: expected an array of bulk strings"), true),
        };
        let mut out = Vec::new();
        reply.encode(&mut out);
        w.write_all(&out)?;
        if close {
            break;
        }
    }
    Ok(())
}

pub fn run() -> std::io::Result<()> {
    let listen = http::listen_addr(&util::env_or("CACHE_LISTEN", ":6379"));
    let password = util::env_opt("REDIS_PASSWORD");
    let store = Arc::new(Store::new(password.clone()));
    {
        let store = Arc::clone(&store);
        http::spawn_server(&util::env_or("STATS_LISTEN", "127.0.0.1:6380"), move |req| match req.path() {
            "/stats" => Response::text(200, &store.stats_text()),
            _ => Response::text(404, "not found\n"),
        })?;
    }
    let listener = TcpListener::bind(&listen)?;
    println!("[cache] listening on {listen} (auth {})", if password.is_some() { "required" } else { "off" });
    for stream in listener.incoming() {
        match stream {
            Ok(s) => {
                let store = Arc::clone(&store);
                std::thread::spawn(move || {
                    let _ = handle(s, &store);
                });
            }
            Err(e) => eprintln!("[cache] accept: {e}"),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cmd(s: &Store, authed: &mut bool, words: &[&str]) -> Value {
        let args: Vec<Vec<u8>> = words.iter().map(|w| w.as_bytes().to_vec()).collect();
        s.execute(&args, authed).0
    }

    #[test]
    fn auth_is_required_when_a_password_is_set() {
        let s = Store::new(Some("pw".into()));
        let mut authed = false;
        assert_eq!(cmd(&s, &mut authed, &["PING"]), Value::Simple("PONG".into()));
        assert!(matches!(cmd(&s, &mut authed, &["GET", "k"]), Value::Error(e) if e.starts_with("NOAUTH")));
        assert!(matches!(cmd(&s, &mut authed, &["AUTH", "nope"]), Value::Error(_)));
        assert_eq!(cmd(&s, &mut authed, &["AUTH", "pw"]), Value::ok());
        assert_eq!(cmd(&s, &mut authed, &["GET", "k"]), Value::Bulk(None));
    }

    #[test]
    fn set_get_expire() {
        let s = Store::new(None);
        let mut a = false;
        assert_eq!(cmd(&s, &mut a, &["SETEX", "k", "30", "v"]), Value::ok());
        assert_eq!(cmd(&s, &mut a, &["GET", "k"]), Value::bulk(b"v"));
        assert!(matches!(cmd(&s, &mut a, &["TTL", "k"]), Value::Int(n) if (29..=30).contains(&n)));
        assert_eq!(cmd(&s, &mut a, &["SET", "p", "1", "PX", "1"]), Value::ok());
        std::thread::sleep(Duration::from_millis(5));
        assert_eq!(cmd(&s, &mut a, &["GET", "p"]), Value::Bulk(None));
        assert_eq!(cmd(&s, &mut a, &["INCR", "n"]), Value::Int(1));
        assert_eq!(cmd(&s, &mut a, &["INCR", "n"]), Value::Int(2));
        assert_eq!(cmd(&s, &mut a, &["DEL", "k", "n", "zz"]), Value::Int(2));
        assert_eq!(cmd(&s, &mut a, &["DBSIZE"]), Value::Int(0));
    }

    #[test]
    fn serves_the_client_over_loopback() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let store = Arc::new(Store::new(Some("pw".into())));
        std::thread::spawn(move || {
            for s in listener.incoming().flatten() {
                let store = Arc::clone(&store);
                std::thread::spawn(move || {
                    let _ = handle(s, &store);
                });
            }
        });
        let c = crate::resp::Client::new("127.0.0.1", port, Some("pw".into()), Duration::from_secs(2));
        c.setex("storefront:catalog", 30, b"{}").unwrap();
        assert_eq!(c.get("storefront:catalog").unwrap(), Some(b"{}".to_vec()));
        let bad = crate::resp::Client::new("127.0.0.1", port, Some("wrong".into()), Duration::from_secs(2));
        assert!(bad.get("storefront:catalog").is_err());
    }
}
