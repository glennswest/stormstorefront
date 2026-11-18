//! stormstorefront-test: `/test short|medium|long` (stormcentral
//! docs/test-standard.md). One JSON line per test on stdout, a summary line,
//! exit 0 (all passed), 1 (a test failed) or 2 (could not run).
//!
//! - `short`: the manifests validate offline, and the apiserver serves every
//!   kind they use.
//! - `medium`: the storefront deployed into the run's own project
//!   (`extra_namespaces = ["storefront"]`) exactly as `sc apply -k manifests/`
//!   makes it, every pod n/n ready, the report of every pod with its
//!   containers, every path through the edge (ClusterIP, Service DNS,
//!   LoadBalancer address, HTTPRoute), requests spread over both edge pods
//!   and every web and api pod from the LoadBalancer address and from the
//!   route's backend config, the route's status, every container's metrics, `logs -c`,
//!   the backup sidecar, an order surviving a `postgres` pod restart, the API
//!   surviving a `cache` restart, a killed pod replaced, `web` scaled
//!   3→6→3 with the edge following, a ConfigMap edit reloading the edge; then
//!   the project's objects deleted and nothing left (no PV, no engine volume
//!   or container clone naming them).
//! - `long`: `medium` with the disruptions repeated in waves, each wave timed.
//!
//! A failure caused by a platform piece that is not there yet is a *gap*: it
//! fails like any other, and its detail and JSON line name the issue that
//! closes it (`"gap": "rustkube#321"` — no UID assignment for restricted
//! pods; `"gap": "stormlb#37"` — LoadBalancer addresses, multiple frontends,
//! the HTTPRoute as backend config). Once a release carries the fix, the same
//! test passes as written.

mod kube;
mod manifests;

use kube::{collection, Kube};
use manifests::Overlay;
use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::{Duration, Instant};
use storefront::http;

enum Bad {
    Fail(String),
    Skip(String),
    /// A failure whose cause is a platform issue not yet released.
    Gap(&'static str, String),
}

/// The platform issues the test names as gaps.
const GAP_UID: &str = "rustkube#321";
const GAP_LB: &str = "stormlb#37";

fn gap<T>(issue: &'static str, msg: impl Into<String>) -> Result<T, Bad> {
    Err(Bad::Gap(issue, msg.into()))
}

/// A pod refused because nothing gave it a non-root UID is rustkube#321's
/// gap, not the storefront's failure (the manifests pin no UID, by design).
fn uid_gap(e: Bad) -> Bad {
    match e {
        Bad::Fail(m) if m.contains("runAsNonRoot") || m.contains("would run as root") => Bad::Gap(GAP_UID, m),
        other => other,
    }
}

type Step = Result<String, Bad>;

fn fail<T>(msg: impl Into<String>) -> Result<T, Bad> {
    Err(Bad::Fail(msg.into()))
}

#[derive(Default)]
struct Tally {
    pass: u32,
    fail: u32,
    skip: u32,
    lines: Vec<Value>,
}

impl Tally {
    fn record(&mut self, test: &str, status: &str, ms: u128, detail: &str) {
        self.record_gap(test, status, ms, detail, None)
    }

    fn record_gap(&mut self, test: &str, status: &str, ms: u128, detail: &str, gap: Option<&str>) {
        match status {
            "pass" => self.pass += 1,
            "fail" => self.fail += 1,
            _ => self.skip += 1,
        }
        let mut line = json!({"test": test, "status": status, "ms": ms as u64, "detail": detail});
        if let Some(g) = gap {
            line["gap"] = json!(g);
        }
        println!("{line}");
        let _ = std::io::stdout().flush();
        self.lines.push(line);
    }

    /// Runs one test and records it; returns whether it passed.
    fn run(&mut self, test: &str, f: impl FnOnce() -> Step) -> bool {
        let t = Instant::now();
        let r = f();
        let ms = t.elapsed().as_millis();
        match r {
            Ok(detail) => {
                self.record(test, "pass", ms, &detail);
                true
            }
            Err(Bad::Fail(detail)) => {
                self.record(test, "fail", ms, &detail);
                false
            }
            Err(Bad::Skip(detail)) => {
                self.record(test, "skip", ms, &detail);
                false
            }
            Err(Bad::Gap(issue, detail)) => {
                self.record_gap(test, "fail", ms, &format!("gap {issue}: {detail}"), Some(issue));
                false
            }
        }
    }
}

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

fn results_dir() -> PathBuf {
    PathBuf::from(env("STORM_RESULTS").unwrap_or_else(|| "/results".into()))
}

fn write_result(name: &str, text: &str) {
    let dir = results_dir();
    let _ = std::fs::create_dir_all(&dir);
    let _ = std::fs::write(dir.join(name), text);
}

// ---- short ----------------------------------------------------------------

fn validate_offline() -> Step {
    let files: Vec<(String, String)> = manifests::FILES.iter().map(|(f, t)| (f.to_string(), t.to_string())).collect();
    let r = storefront::validate::validate(&files, storefront::ROLES);
    write_result("validate.txt", &r.render());
    if r.ok() {
        Ok(r.render().lines().next().unwrap_or("").to_string())
    } else {
        fail(r.render())
    }
}

/// The kinds the manifests use, by API group version and plural.
const KINDS: &[(&str, &str)] = &[
    ("/api/v1", "resourcequotas"),
    ("/api/v1", "limitranges"),
    ("/api/v1", "serviceaccounts"),
    ("/api/v1", "secrets"),
    ("/api/v1", "configmaps"),
    ("/api/v1", "services"),
    ("/api/v1", "persistentvolumeclaims"),
    ("/apis/apps/v1", "deployments"),
    ("/apis/apps/v1", "statefulsets"),
    ("/apis/rbac.authorization.k8s.io/v1", "roles"),
    ("/apis/rbac.authorization.k8s.io/v1", "rolebindings"),
    ("/apis/networking.k8s.io/v1", "networkpolicies"),
    ("/apis/autoscaling/v2", "horizontalpodautoscalers"),
    ("/apis/policy/v1", "poddisruptionbudgets"),
    ("/apis/gateway.networking.k8s.io/v1", "httproutes"),
];

fn api_kinds(k: &Kube) -> Step {
    let mut missing = Vec::new();
    for (gv, plural) in KINDS {
        match k.get(gv) {
            Ok((200, v)) => {
                let served = v["resources"].as_array().is_some_and(|rs| rs.iter().any(|r| r["name"] == *plural));
                if !served {
                    missing.push(format!("{gv} {plural}"));
                }
            }
            Ok((s, _)) => missing.push(format!("{gv} ({s})")),
            Err(e) => return Err(Bad::Fail(e)),
        }
    }
    if missing.is_empty() {
        Ok(format!("all {} kinds served", KINDS.len()))
    } else {
        fail(format!("not served: {}", missing.join(", ")))
    }
}

// ---- medium ---------------------------------------------------------------

struct Ctx {
    k: Kube,
    ns: String,
    node: Option<String>,
    overlay: Overlay,
    created: Vec<(String, String)>,
    container_ids: BTreeSet<String>,
    pv_names: BTreeSet<String>,
    edge_ip: Option<String>,
    lb_ip: Option<String>,
    order_id: Option<u64>,
}

/// One way in: the edge's ClusterIP, the LoadBalancer address, or the route
/// (a node's :80 with the route's Host).
#[derive(Clone)]
struct Front {
    label: &'static str,
    base: String,
    host: Option<String>,
}

impl Front {
    fn get(&self, path: &str) -> Result<http::Response, String> {
        let headers: Vec<(&str, &str)> = self.host.as_deref().map(|h| vec![("Host", h)]).unwrap_or_default();
        http::call("GET", &format!("{}{path}", self.base), &headers, b"", Duration::from_secs(10)).map_err(|e| e.to_string())
    }

    /// GETs `path` until `header` has named `want` distinct pods, or `max`
    /// requests. Returns the count per pod, the failed requests and how many
    /// answers carried `absent` (a header that must not be there).
    fn spread(&self, path: &str, header: &str, want: usize, max: usize, absent: Option<&str>) -> Spread {
        let mut sp = Spread::default();
        for _ in 0..max {
            match self.get(path) {
                Ok(r) if r.status == 200 => {
                    if let Some(p) = r.header(header) {
                        *sp.pods.entry(p.to_string()).or_default() += 1;
                    }
                    if absent.is_some_and(|a| r.header(a).is_some()) {
                        sp.via_absent += 1;
                    }
                }
                Ok(r) => sp.failed.push(format!("{} {}", r.status, r.body_str().chars().take(120).collect::<String>())),
                Err(e) => sp.failed.push(e),
            }
            if sp.pods.len() >= want {
                break;
            }
        }
        sp
    }
}

#[derive(Default)]
struct Spread {
    pods: std::collections::BTreeMap<String, u32>,
    failed: Vec<String>,
    via_absent: u32,
}

impl Spread {
    fn show(&self) -> String {
        let pods: Vec<String> = self.pods.iter().map(|(p, n)| format!("{p}={n}")).collect();
        let mut s = format!("[{}]", pods.join(" "));
        if !self.failed.is_empty() {
            s.push_str(&format!(", {} failed (first: {})", self.failed.len(), self.failed[0]));
        }
        s
    }
}

fn pod_ready(p: &Value) -> bool {
    if !p["metadata"]["deletionTimestamp"].is_null() || p["status"]["phase"] != "Running" {
        return false;
    }
    let want = p["spec"]["containers"].as_array().map(|c| c.len()).unwrap_or(0);
    let st = p["status"]["containerStatuses"].as_array().cloned().unwrap_or_default();
    want > 0 && st.len() == want && st.iter().all(|c| c["ready"] == true)
}

fn state_of(c: &Value) -> String {
    let st = &c["state"];
    if let Some(w) = st["waiting"].as_object() {
        format!("waiting {} {}", w.get("reason").and_then(Value::as_str).unwrap_or(""), w.get("message").and_then(Value::as_str).unwrap_or(""))
    } else if let Some(t) = st["terminated"].as_object() {
        format!(
            "terminated {} (exit {})",
            t.get("reason").and_then(Value::as_str).unwrap_or(""),
            t.get("exitCode").and_then(Value::as_i64).unwrap_or(-1)
        )
    } else if !st["running"].is_null() {
        "running".into()
    } else {
        "unknown".into()
    }
}

/// `sc get pods` with every container under its pod.
fn pods_table(pods: &[Value]) -> String {
    let mut out = format!("{:<34} {:<6} {:<10} {:<8}\n", "NAME", "READY", "STATUS", "RESTARTS");
    for p in pods {
        let st = p["status"]["containerStatuses"].as_array().cloned().unwrap_or_default();
        let ready = st.iter().filter(|c| c["ready"] == true).count();
        let want = p["spec"]["containers"].as_array().map(|c| c.len()).unwrap_or(0);
        let restarts: i64 = st.iter().map(|c| c["restartCount"].as_i64().unwrap_or(0)).sum();
        out.push_str(&format!(
            "{:<34} {:<6} {:<10} {:<8}\n",
            p["metadata"]["name"].as_str().unwrap_or("?"),
            format!("{ready}/{want}"),
            p["status"]["phase"].as_str().unwrap_or("?"),
            restarts
        ));
        for c in p["status"]["initContainerStatuses"].as_array().cloned().unwrap_or_default() {
            out.push_str(&format!("    init  {:<18} {}\n", c["name"].as_str().unwrap_or("?"), state_of(&c)));
        }
        for c in &st {
            out.push_str(&format!(
                "    ctr   {:<18} {} ready={} restarts={}\n",
                c["name"].as_str().unwrap_or("?"),
                state_of(c),
                c["ready"],
                c["restartCount"]
            ));
        }
    }
    out
}

fn metric(text: &str, name_and_labels: &str) -> Option<f64> {
    text.lines().find_map(|l| l.strip_prefix(name_and_labels).and_then(|rest| rest.trim().parse().ok()))
}

impl Ctx {
    fn pods(&self, app: &str) -> Vec<Value> {
        self.k.items(&format!("/api/v1/namespaces/{}/pods?labelSelector=app.kubernetes.io%2Fname%3D{app}", self.ns))
    }

    fn all_pods(&self) -> Vec<Value> {
        let mut pods = self.k.items(&format!("/api/v1/namespaces/{}/pods", self.ns));
        pods.sort_by(|a, b| a["metadata"]["name"].as_str().cmp(&b["metadata"]["name"].as_str()));
        pods
    }

    fn remember(&mut self, pods: &[Value]) {
        for p in pods {
            for key in ["containerStatuses", "initContainerStatuses"] {
                for c in p["status"][key].as_array().cloned().unwrap_or_default() {
                    if let Some(id) = c["containerID"].as_str() {
                        let bare = id.rsplit("://").next().unwrap_or(id);
                        if bare.len() >= 12 {
                            self.container_ids.insert(bare.to_string());
                        }
                    }
                }
            }
        }
    }

    /// Waits until `app` has exactly `n` pods, all n/n ready.
    fn wait_ready(&mut self, app: &str, n: usize, timeout: Duration) -> Step {
        let t = Instant::now();
        loop {
            let pods: Vec<Value> = self.pods(app).into_iter().filter(|p| p["metadata"]["deletionTimestamp"].is_null()).collect();
            self.remember(&pods);
            if pods.len() == n && pods.iter().all(pod_ready) {
                let names: Vec<&str> = pods.iter().filter_map(|p| p["metadata"]["name"].as_str()).collect();
                return Ok(format!("{n}/{n} pods ready in {}s: {}", t.elapsed().as_secs(), names.join(" ")));
            }
            if t.elapsed() > timeout {
                return fail(format!("{app}: not {n} ready pods after {}s\n{}", timeout.as_secs(), pods_table(&pods)));
            }
            std::thread::sleep(Duration::from_secs(2));
        }
    }

    fn create_file(&mut self, file: &str) -> Step {
        let mut made = Vec::new();
        for obj in manifests::objects(file).map_err(Bad::Fail)? {
            let Some(obj) = self.overlay.apply(obj) else { continue };
            let (av, kind) = (obj["apiVersion"].as_str().unwrap_or(""), obj["kind"].as_str().unwrap_or(""));
            let name = obj["metadata"]["name"].as_str().unwrap_or("").to_string();
            let Some(path) = collection(av, kind, &self.ns) else {
                return fail(format!("{file}: the test does not know kind {kind}"));
            };
            match self.k.post(&path, &obj) {
                Ok((200..=202, _)) | Ok((409, _)) => {
                    self.created.push((path, name.clone()));
                    made.push(format!("{kind}/{name}"));
                }
                Ok((s, v)) => {
                    let msg = v["message"].as_str().map(str::to_string).unwrap_or_else(|| v.to_string());
                    return fail(format!("{file}: create {kind}/{name}: {s} {msg}"));
                }
                Err(e) => return fail(format!("{file}: create {kind}/{name}: {e}")),
            }
        }
        Ok(made.join(" "))
    }

    fn edge_base(&mut self) -> Result<String, Bad> {
        if let Some(ip) = &self.edge_ip {
            return Ok(format!("http://{ip}:80"));
        }
        match self.k.get(&format!("/api/v1/namespaces/{}/services/edge-lb", self.ns)) {
            Ok((200, v)) => match v["spec"]["clusterIP"].as_str() {
                Some(ip) if !ip.is_empty() && ip != "None" => {
                    self.edge_ip = Some(ip.to_string());
                    Ok(format!("http://{ip}:80"))
                }
                _ => fail("edge-lb has no clusterIP"),
            },
            Ok((s, _)) => fail(format!("edge-lb: {s}")),
            Err(e) => fail(e),
        }
    }

    fn edge(&mut self, method: &str, path: &str, body: &[u8]) -> Result<http::Response, Bad> {
        let base = self.edge_base()?;
        http::call(method, &format!("{base}{path}"), &[("Content-Type", "application/json")], body, Duration::from_secs(10))
            .map_err(|e| Bad::Fail(format!("{method} {path} through the edge: {e}")))
    }

    fn pod_log(&self, pod: &str, container: &str) -> String {
        match self.k.get_text(&format!("/api/v1/namespaces/{}/pods/{pod}/log?container={container}", self.ns)) {
            Ok((200, t)) => t,
            Ok((s, t)) => format!("<log answered {s}: {t}>"),
            Err(e) => format!("<log failed: {e}>"),
        }
    }

    fn pod_ips(&self, app: &str) -> Vec<(String, String)> {
        self.pods(app)
            .iter()
            .filter(|p| pod_ready(p))
            .filter_map(|p| Some((p["metadata"]["name"].as_str()?.to_string(), p["status"]["podIP"].as_str()?.to_string())))
            .collect()
    }

    fn delete_pod(&self, name: &str) -> Result<String, Bad> {
        let uid = match self.k.get(&format!("/api/v1/namespaces/{}/pods/{name}", self.ns)) {
            Ok((200, v)) => v["metadata"]["uid"].as_str().unwrap_or("").to_string(),
            other => return fail(format!("get pod {name}: {other:?}")),
        };
        match self.k.delete(&format!("/api/v1/namespaces/{}/pods/{name}", self.ns)) {
            Ok((200 | 202, _)) => Ok(uid),
            other => fail(format!("delete pod {name}: {other:?}")),
        }
    }

    fn scale(&self, deployment: &str, n: u64) -> Step {
        match self.k.merge_patch(&format!("/apis/apps/v1/namespaces/{}/deployments/{deployment}", self.ns), &json!({"spec": {"replicas": n}})) {
            Ok((200, _)) => Ok(format!("{deployment} scaled to {n}")),
            other => fail(format!("scale {deployment}: {other:?}")),
        }
    }

    /// The LoadBalancer address, waiting up to `wait` for stormlb to give one.
    fn lb_front(&mut self, wait: Duration) -> Option<Front> {
        let t0 = Instant::now();
        while self.lb_ip.is_none() {
            if let Ok((200, v)) = self.k.get(&format!("/api/v1/namespaces/{}/services/edge-lb", self.ns)) {
                let ing = &v["status"]["loadBalancer"]["ingress"][0];
                if let Some(ip) = ing["ip"].as_str().or(ing["hostname"].as_str()) {
                    self.lb_ip = Some(ip.to_string());
                    break;
                }
            }
            if t0.elapsed() >= wait {
                return None;
            }
            std::thread::sleep(Duration::from_secs(3));
        }
        self.lb_ip.as_ref().map(|ip| Front { label: "loadbalancer", base: format!("http://{ip}"), host: None })
    }

    /// The route: the run's Host on the node's :80 (stormlb's router).
    fn route_front(&self) -> Option<Front> {
        let node = self.node.as_ref()?;
        Some(Front { label: "httproute", base: format!("http://{node}:80"), host: Some(self.overlay.hostname.clone()) })
    }

    /// Every edge pod reports `n` active servers for `backend`.
    fn edges_see(&self, backend: &str, n: usize, timeout: Duration) -> Step {
        let t = Instant::now();
        let key = format!("storefront_edge_backend_active_servers{{backend=\"{backend}\"}}");
        loop {
            let mut seen = Vec::new();
            for (pod, ip) in self.pod_ips("edge") {
                let v = http::get(&format!("http://{ip}:8404/metrics"), Duration::from_secs(5))
                    .ok()
                    .and_then(|r| metric(&r.body_str(), &key))
                    .unwrap_or(-1.0);
                seen.push((pod, v));
            }
            if !seen.is_empty() && seen.iter().all(|(_, v)| *v as usize == n && *v >= 0.0) {
                return Ok(format!("every edge pod sees {n} {backend} servers after {}s", t.elapsed().as_secs()));
            }
            if t.elapsed() > timeout {
                return fail(format!("edge pods see {seen:?} {backend} servers, not {n}"));
            }
            std::thread::sleep(Duration::from_secs(2));
        }
    }
}

/// A deploy stage: its test name, the files it applies, the apps it waits for (and how many pods).
type Stage = (&'static str, &'static [&'static str], &'static [(&'static str, usize)]);

const READY_TIMEOUT: Duration = Duration::from_secs(300);

fn deploy(t: &mut Tally, c: &mut Ctx) -> bool {
    // Dependency order, as scripts/deploy.sh: config, the data tier (the
    // API's init container waits on it), api, web, edge, then the policies.
    let stages: &[Stage] = &[
        (
            "deploy-config",
            &["01-quota-limits.yaml", "02-rbac.yaml", "03-secrets.yaml", "04-config-edge.yaml", "05-config-web.yaml", "06-config-api.yaml"],
            &[],
        ),
        ("deploy-data", &["10-data-postgres.yaml", "11-data-cache.yaml"], &[("postgres", 1), ("cache", 1)]),
        ("deploy-api", &["20-api.yaml"], &[("api", 3)]),
        ("deploy-web", &["30-web.yaml"], &[("web", 3)]),
        ("deploy-edge", &["40-edge.yaml"], &[("edge", 2)]),
        ("deploy-policy", &["50-networkpolicy.yaml", "60-autoscale-pdb.yaml", "70-monitoring.yaml"], &[]),
    ];
    for (name, files, waits) in stages {
        let ok = t.run(name, || {
            let mut made = Vec::new();
            for f in *files {
                made.push(c.create_file(f)?);
            }
            let mut ready = Vec::new();
            for (app, n) in *waits {
                ready.push(c.wait_ready(app, *n, READY_TIMEOUT).map_err(uid_gap)?);
            }
            Ok(format!("created {}; {}", made.join(" "), ready.join("; ")))
        });
        if !ok {
            return false;
        }
    }
    true
}

fn checks(t: &mut Tally, c: &mut Ctx) {
    t.run("objects", || {
        // sc get hpa,pdb,networkpolicy,httproute,svc -n <project>
        let want = [
            ("/apis/autoscaling/v2", "horizontalpodautoscalers", 2),
            ("/apis/policy/v1", "poddisruptionbudgets", 3),
            ("/apis/networking.k8s.io/v1", "networkpolicies", 7),
            ("/apis/gateway.networking.k8s.io/v1", "httproutes", 1),
            ("/api/v1", "services", 9),
            ("/api/v1", "persistentvolumeclaims", 2),
        ];
        let mut got = Vec::new();
        let mut bad = Vec::new();
        for (gv, plural, n) in want {
            let items = c.k.items(&format!("{gv}/namespaces/{}/{plural}", c.ns));
            got.push(format!("{plural}={}", items.len()));
            if items.len() != n {
                bad.push(format!("{plural}: {} not {n}", items.len()));
            }
        }
        if bad.is_empty() {
            Ok(got.join(" "))
        } else {
            fail(bad.join("; "))
        }
    });

    t.run("pods", || {
        let pods = c.all_pods();
        c.remember(&pods);
        let table = pods_table(&pods);
        write_result("pods.txt", &table);
        let ctrs: usize = pods.iter().map(|p| p["spec"]["containers"].as_array().map(|x| x.len()).unwrap_or(0)).sum();
        let inits: usize = pods.iter().map(|p| p["spec"]["initContainers"].as_array().map(|x| x.len()).unwrap_or(0)).sum();
        let all_ready = pods.iter().all(pod_ready);
        let multi = pods.iter().all(|p| p["status"]["containerStatuses"].as_array().map(|x| x.len()).unwrap_or(0) >= 2);
        let inits_done = pods.iter().all(|p| {
            p["status"]["initContainerStatuses"]
                .as_array()
                .map(|cs| cs.iter().all(|c| c["state"]["terminated"]["exitCode"] == 0))
                .unwrap_or(true)
        });
        if pods.len() == 10 && ctrs == 27 && inits == 8 && all_ready && multi && inits_done {
            Ok(format!("10 pods, 27 containers ready, 8 init containers completed\n{table}"))
        } else {
            fail(format!(
                "{} pods, {ctrs} containers, {inits} init (want 10/27/8); all ready {all_ready}, every pod >1 container {multi}, inits done {inits_done}\n{table}",
                pods.len()
            ))
        }
    });

    t.run("smoke", || {
        let paths: &[(&str, &str, &str)] = &[
            ("edge /healthz", "/healthz", "ok"),
            ("GET /", "/", "Storefront"),
            ("content revision", "/version.json", "revision"),
            ("GET /api/healthz", "/api/healthz", "\"status\": \"ok\""),
            ("GET /api/readyz", "/api/readyz", "\"status\": \"ready\""),
            ("GET /api/products", "/api/products", "ANV-001"),
            ("api metrics", "/api/metrics", "storefront_api_requests_total"),
        ];
        let mut out = Vec::new();
        for (label, path, expect) in paths {
            let r = c.edge("GET", path, b"")?;
            if r.status != 200 || !r.body_str().contains(expect) {
                return fail(format!("{label}: {} {}", r.status, r.body_str().chars().take(300).collect::<String>()));
            }
            if r.header("X-Served-By").is_none() {
                return fail(format!("{label}: no X-Served-By from the edge"));
            }
            out.push(format!("PASS {label}"));
        }
        // The cache is really used: a second read comes from it.
        let mut cached = false;
        for _ in 0..5 {
            if c.edge("GET", "/api/products", b"")?.body_str().contains("\"source\": \"cache\"") {
                cached = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(500));
        }
        if !cached {
            return fail("the catalog never came from the cache");
        }
        out.push("PASS catalog served from cache".into());
        // Balancing across replicas, per pod, by the edge.
        let mut api_pods = BTreeSet::new();
        let mut web_pods = BTreeSet::new();
        for _ in 0..20 {
            let r = c.edge("GET", "/api/products", b"")?;
            if let Some(p) = r.header("X-Api-Pod") {
                api_pods.insert(p.to_string());
            }
            let r = c.edge("GET", "/", b"")?;
            if let Some(p) = r.header("X-Web-Pod") {
                web_pods.insert(p.to_string());
            }
        }
        if api_pods.len() < 2 || web_pods.len() < 2 {
            return fail(format!("20 requests reached api pods {api_pods:?}, web pods {web_pods:?}: not balanced"));
        }
        out.push(format!("PASS 20 requests reached {} api and {} web pods", api_pods.len(), web_pods.len()));
        Ok(out.join("; "))
    });

    t.run("service-dns", || {
        let url = format!("http://edge-lb.{}.svc.cluster.local/healthz", c.ns);
        match http::get(&url, Duration::from_secs(10)) {
            Ok(r) if r.status == 200 => Ok(format!("{url} answered through cluster DNS")),
            Ok(r) => fail(format!("{url}: {}", r.status)),
            Err(e) => fail(format!("{url}: {e}")),
        }
    });

    t.run("loadbalancer", || {
        // stormlb allocates the address from the node's lb_pool (stormlb#37).
        let Some(lb) = c.lb_front(Duration::from_secs(60)) else {
            return gap(GAP_LB, "edge-lb has no LoadBalancer address after 60s (status.loadBalancer.ingress empty)");
        };
        let r = lb.get("/api/products").map_err(|e| Bad::Gap(GAP_LB, format!("{}: {e}", lb.base)))?;
        if r.status == 200 && r.body_str().contains("ANV-001") {
            Ok(format!("{}/api/products answered", lb.base))
        } else {
            gap(GAP_LB, format!("{}/api/products: {}", lb.base, r.status))
        }
    });

    t.run("loadbalancer-spread", || {
        // Multiple frontends: both edge pods take the address's traffic, and
        // each spreads it over every web and api pod.
        let Some(lb) = c.lb_front(Duration::ZERO) else { return gap(GAP_LB, "edge-lb has no LoadBalancer address") };
        let edges = c.pod_ips("edge").len();
        let edge = lb.spread("/", "X-Served-By", edges, 120, None);
        if edge.pods.len() < edges || !edge.failed.is_empty() {
            return gap(GAP_LB, format!("{}/ reached edge pods {} of {edges}", lb.base, edge.show()));
        }
        let web = lb.spread("/", "X-Web-Pod", 3, 120, None);
        let api = lb.spread("/api/products", "X-Api-Pod", 3, 120, None);
        if web.pods.len() < 3 || api.pods.len() < 3 || !web.failed.is_empty() || !api.failed.is_empty() {
            return fail(format!("through {}: web {} api {} (want 3 each)", lb.base, web.show(), api.show()));
        }
        Ok(format!("{}: edge {} web {} api {}", lb.base, edge.show(), web.show(), api.show()))
    });

    t.run("httproute", || {
        let Some(node) = c.node.clone() else { return Err(Bad::Skip("STORM_NODE is not set".into())) };
        let host = c.overlay.hostname.clone();
        let t0 = Instant::now();
        let mut last = String::new();
        while t0.elapsed() < Duration::from_secs(90) {
            match http::call("GET", &format!("http://{node}:80/"), &[("Host", host.as_str())], b"", Duration::from_secs(10)) {
                Ok(r) if r.status == 200 && r.body_str().contains("Storefront") => {
                    return Ok(format!("Host {host} on {node}:80 reached the storefront through stormlb after {}s", t0.elapsed().as_secs()));
                }
                Ok(r) => last = format!("{} {}", r.status, r.body_str().chars().take(200).collect::<String>()),
                Err(e) => last = e.to_string(),
            }
            std::thread::sleep(Duration::from_secs(3));
        }
        fail(format!("Host {host} on {node}:80: {last}"))
    });

    t.run("httproute-backends", || {
        // The route's rules are stormlb's backend config: `/` -> web and
        // `/api/` -> api straight to the pods (no web hop), each spread over
        // every ready pod, and the route's status written back.
        let Some(route) = c.route_front() else { return Err(Bad::Skip("STORM_NODE is not set".into())) };
        let mut gaps = Vec::new();
        let web = route.spread("/", "X-Web-Pod", 3, 120, None);
        if web.pods.len() < 3 || !web.failed.is_empty() {
            gaps.push(format!("/ reached web pods {} (want 3)", web.show()));
        }
        let api = route.spread("/api/products", "X-Api-Pod", 3, 120, Some("X-Web-Pod"));
        if api.pods.len() < 3 || !api.failed.is_empty() {
            gaps.push(format!("/api/ reached api pods {} (want 3)", api.show()));
        }
        if api.via_absent > 0 {
            gaps.push(format!("{} /api/ answers came through web: the router does not apply the /api/ rule", api.via_absent));
        }
        let status = match c.k.get(&format!("/apis/gateway.networking.k8s.io/v1/namespaces/{}/httproutes/storefront", c.ns)) {
            Ok((200, v)) => v["status"].clone(),
            other => return fail(format!("get httproute storefront: {other:?}")),
        };
        let conds: Vec<String> = status["parents"]
            .as_array()
            .into_iter()
            .flatten()
            .flat_map(|p| p["conditions"].as_array().cloned().unwrap_or_default())
            .map(|c| format!("{}={}", c["type"].as_str().unwrap_or("?"), c["status"].as_str().unwrap_or("?")))
            .collect();
        for want in ["Accepted=True", "ResolvedRefs=True"] {
            if !conds.iter().any(|c| c == want) {
                gaps.push(format!("route status lacks {want} (has {conds:?})"));
            }
        }
        write_result("httproute-status.json", &serde_json::to_string_pretty(&status).unwrap_or_default());
        if !gaps.is_empty() {
            return gap(GAP_LB, gaps.join("; "));
        }
        Ok(format!("web {} api {} (direct); status {}", web.show(), api.show(), conds.join(" ")))
    });

    t.run("metrics", || {
        // Every container that serves metrics, on every pod, by pod IP.
        let targets: &[(&str, u16, &str, &str)] = &[
            ("edge", 8404, "/metrics", "storefront_edge_backend_active_servers"),
            ("web", 9113, "/metrics", "storefront_web_up{"),
            ("api", 8081, "/api/metrics", "storefront_api_requests_total"),
            ("postgres", 9187, "/metrics", "storefront_store_up{"),
            ("cache", 9121, "/metrics", "storefront_cache_up{"),
        ];
        let mut ok = Vec::new();
        let mut bad = Vec::new();
        for (app, port, path, needle) in targets {
            for (pod, ip) in c.pod_ips(app) {
                match http::get(&format!("http://{ip}:{port}{path}"), Duration::from_secs(5)) {
                    Ok(r) if r.status == 200 && r.body_str().contains(needle) => {
                        // An exporter that cannot reach its neighbour says up 0.
                        if needle.ends_with("_up{") && !r.body_str().lines().any(|l| l.starts_with(needle) && l.ends_with(" 1")) {
                            bad.push(format!("{pod}:{port} reports its neighbour down"));
                        } else {
                            ok.push(format!("{pod}:{port}"));
                        }
                    }
                    Ok(r) => bad.push(format!("{pod}:{port}{path} {}", r.status)),
                    Err(e) => bad.push(format!("{pod}:{port}{path} {e}")),
                }
            }
        }
        if bad.is_empty() && ok.len() == 11 {
            Ok(format!("{} metrics endpoints: {}", ok.len(), ok.join(" ")))
        } else {
            fail(format!("ok {ok:?}; bad {bad:?} (want 11)"))
        }
    });

    t.run("logs", || {
        // sc logs <pod> -c <container>, for a sidecar of each kind.
        let api = c.pods("api");
        let pod = api.first().and_then(|p| p["metadata"]["name"].as_str()).unwrap_or("").to_string();
        let _ = c.edge("GET", "/api/products", b"");
        let t0 = Instant::now();
        loop {
            let log = c.pod_log(&pod, "access-log");
            let warm = c.pod_log(&pod, "cache-warmer");
            if log.contains("/api/") && warm.contains("[warm]") {
                return Ok(format!("{pod} -c access-log shows the API's requests; -c cache-warmer shows its warms"));
            }
            if t0.elapsed() > Duration::from_secs(60) {
                return fail(format!("{pod}: access-log {:?}; cache-warmer {:?}", tail(&log), tail(&warm)));
            }
            std::thread::sleep(Duration::from_secs(3));
        }
    });

    t.run("backup", || {
        let t0 = Instant::now();
        loop {
            let log = c.pod_log("postgres-0", "backup");
            if log.contains("[backup] wrote") {
                return Ok(format!("postgres-0 -c backup: {}", log.lines().find(|l| l.contains("wrote")).unwrap_or("")));
            }
            if t0.elapsed() > Duration::from_secs(120) {
                return fail(format!("no backup written: {:?}", tail(&log)));
            }
            std::thread::sleep(Duration::from_secs(5));
        }
    });
}

fn tail(s: &str) -> String {
    let lines: Vec<&str> = s.lines().collect();
    lines[lines.len().saturating_sub(5)..].join(" | ")
}

/// The disruptions, once per wave. Returns whether all passed.
fn disruptions(t: &mut Tally, c: &mut Ctx, wave: u32) -> bool {
    let sfx = if wave == 0 { String::new() } else { format!("-wave{wave}") };
    let mut all = true;

    all &= t.run(&format!("postgres-restart{sfx}"), || {
        let r = c.edge("POST", "/api/orders", br#"{"sku": "ANV-001", "qty": 2}"#)?;
        if r.status != 201 {
            return fail(format!("POST /api/orders: {} {}", r.status, r.body_str()));
        }
        let order: Value = serde_json::from_slice(&r.body).map_err(|e| Bad::Fail(e.to_string()))?;
        let id = order["id"].as_u64().ok_or_else(|| Bad::Fail(format!("no id in {order}")))?;
        c.order_id = Some(id);
        let old_uid = c.delete_pod("postgres-0")?;
        let t0 = Instant::now();
        loop {
            if let Ok((200, p)) = c.k.get(&format!("/api/v1/namespaces/{}/pods/postgres-0", c.ns)) {
                if p["metadata"]["uid"] != old_uid.as_str() && pod_ready(&p) {
                    break;
                }
            }
            if t0.elapsed() > READY_TIMEOUT {
                return fail("postgres-0 was not replaced and ready in time");
            }
            std::thread::sleep(Duration::from_secs(2));
        }
        // The API may need a moment to see the store again.
        let t1 = Instant::now();
        loop {
            let r = c.edge("GET", &format!("/api/orders/{id}"), b"")?;
            if r.status == 200 && r.body_str().contains("ANV-001") {
                return Ok(format!("order {id} survived the postgres-0 restart (back in {}s)", t0.elapsed().as_secs()));
            }
            if t1.elapsed() > Duration::from_secs(60) {
                return fail(format!("order {id} after the restart: {} {}", r.status, r.body_str()));
            }
            std::thread::sleep(Duration::from_secs(2));
        }
    });

    all &= t.run(&format!("cache-restart{sfx}"), || {
        let restarts = |c: &Ctx| -> i64 {
            c.pods("api").iter().flat_map(|p| p["status"]["containerStatuses"].as_array().cloned().unwrap_or_default()).map(|s| s["restartCount"].as_i64().unwrap_or(0)).sum()
        };
        let api_pods: BTreeSet<String> = c.pods("api").iter().filter_map(|p| p["metadata"]["name"].as_str().map(str::to_string)).collect();
        let before = restarts(c);
        let cache = c.pods("cache").first().and_then(|p| p["metadata"]["name"].as_str().map(str::to_string)).unwrap_or_default();
        c.delete_pod(&cache)?;
        let t0 = Instant::now();
        let (mut ok, mut failed) = (0, 0);
        loop {
            match c.edge("GET", "/api/products", b"") {
                Ok(r) if r.status == 200 => ok += 1,
                _ => failed += 1,
            }
            let cache_back = c.pods("cache").iter().any(|p| pod_ready(p) && p["metadata"]["name"] != cache.as_str());
            if cache_back && c.edge("GET", "/api/products", b"")?.body_str().contains("\"source\": \"cache\"") {
                break;
            }
            if t0.elapsed() > READY_TIMEOUT {
                return fail("the cache did not come back, or the API never used it again");
            }
            std::thread::sleep(Duration::from_millis(500));
        }
        let after: BTreeSet<String> = c.pods("api").iter().filter_map(|p| p["metadata"]["name"].as_str().map(str::to_string)).collect();
        if restarts(c) != before || after != api_pods {
            return fail(format!("the API did not survive: restarts {before} -> {}, pods {api_pods:?} -> {after:?}", restarts(c)));
        }
        Ok(format!(
            "cache {cache} replaced in {}s; the API pods kept running (no restart); {ok} requests answered, {failed} refused while the cache was away",
            t0.elapsed().as_secs()
        ))
    });

    all &= t.run(&format!("pod-replaced{sfx}"), || {
        // Killed under traffic on every way in: the edge's ClusterIP (the
        // storefront's own balancing), and stormlb's address and route.
        let mut fronts = vec![Front { label: "edge", base: c.edge_base()?, host: None }];
        fronts.extend(c.lb_front(Duration::ZERO));
        fronts.extend(c.route_front());
        let web = c.pods("web");
        let victim = web.first().and_then(|p| p["metadata"]["name"].as_str().map(str::to_string)).unwrap_or_default();
        c.delete_pod(&victim)?;
        let t0 = Instant::now();
        let mut tally: Vec<(u32, Vec<String>)> = vec![(0, Vec::new()); fronts.len()];
        loop {
            for (f, (ok, failed)) in fronts.iter().zip(tally.iter_mut()) {
                match f.get("/") {
                    Ok(r) if r.status == 200 => *ok += 1,
                    Ok(r) => failed.push(r.status.to_string()),
                    Err(e) => failed.push(e),
                }
            }
            let pods = c.pods("web");
            let live: Vec<&Value> = pods.iter().filter(|p| p["metadata"]["deletionTimestamp"].is_null()).collect();
            if live.len() == 3 && live.iter().all(|p| pod_ready(p)) && !live.iter().any(|p| p["metadata"]["name"] == victim.as_str()) {
                break;
            }
            if t0.elapsed() > READY_TIMEOUT {
                return fail(format!("web not back to 3 ready pods after deleting {victim}"));
            }
            std::thread::sleep(Duration::from_millis(500));
        }
        let secs = t0.elapsed().as_secs();
        let report: Vec<String> = fronts
            .iter()
            .zip(&tally)
            .map(|(f, (ok, failed))| format!("{}: {ok} answered, {} failed{}", f.label, failed.len(), failed.first().map(|e| format!(" ({e})")).unwrap_or_default()))
            .collect();
        if !tally[0].1.is_empty() {
            return fail(format!("{victim} replaced in {secs}s, but requests failed: {}", report.join("; ")));
        }
        if tally.iter().any(|(_, failed)| !failed.is_empty()) {
            return gap(GAP_LB, format!("{victim} replaced in {secs}s; stormlb did not drain it: {}", report.join("; ")));
        }
        Ok(format!("{victim} killed and replaced in {secs}s with no failed request: {}", report.join("; ")))
    });

    all &= t.run(&format!("scale-web{sfx}"), || {
        let mut out = Vec::new();
        for n in [6usize, 3] {
            out.push(c.scale("web", n as u64)?);
            out.push(c.wait_ready("web", n, READY_TIMEOUT)?);
            out.push(c.edges_see("web", n, Duration::from_secs(90))?);
            // stormlb's pool for the route's `/` rule follows too.
            if let Some(route) = c.route_front() {
                let t0 = Instant::now();
                loop {
                    let sp = route.spread("/", "X-Web-Pod", n, 40 * n, None);
                    let live: BTreeSet<String> = c.pod_ips("web").into_iter().map(|(p, _)| p).collect();
                    let stale: Vec<&String> = sp.pods.keys().filter(|p| !live.contains(*p)).collect();
                    if sp.pods.len() == n && stale.is_empty() && sp.failed.is_empty() {
                        out.push(format!("the route reaches all {n} web pods after {}s", t0.elapsed().as_secs()));
                        break;
                    }
                    if t0.elapsed() > Duration::from_secs(90) {
                        return gap(GAP_LB, format!("the route's pool did not follow web to {n}: {} (stale {stale:?})", sp.show()));
                    }
                    std::thread::sleep(Duration::from_secs(3));
                }
            }
        }
        Ok(out.join("; "))
    });
    all
}

fn config_reload(t: &mut Tally, c: &mut Ctx) {
    t.run("edge-config-reload", || {
        let path = format!("/api/v1/namespaces/{}/configmaps/edge-config", c.ns);
        let cm = match c.k.get(&path) {
            Ok((200, v)) => v,
            other => return fail(format!("get edge-config: {other:?}")),
        };
        let text = cm["data"]["edge.toml"].as_str().unwrap_or("").to_string();
        let edited = text.replace("resolve_interval_ms = 5000", "resolve_interval_ms = 4000");
        if edited == text {
            return fail("edge.toml has no resolve_interval_ms = 5000 to edit");
        }
        match c.k.merge_patch(&path, &json!({"data": {"edge.toml": edited}})) {
            Ok((200, _)) => {}
            other => return fail(format!("patch edge-config: {other:?}")),
        }
        let t0 = Instant::now();
        loop {
            let mut reloaded = Vec::new();
            let edges = c.pod_ips("edge");
            for (pod, ip) in &edges {
                let n = http::get(&format!("http://{ip}:8404/metrics"), Duration::from_secs(5))
                    .ok()
                    .and_then(|r| metric(&r.body_str(), "storefront_edge_reloads_total"))
                    .unwrap_or(0.0);
                if n >= 1.0 {
                    reloaded.push(pod.clone());
                }
            }
            if !edges.is_empty() && reloaded.len() == edges.len() {
                let how: Vec<String> = edges
                    .iter()
                    .map(|(pod, _)| {
                        let log = c.pod_log(pod, "config-reloader");
                        let via = if log.contains("reload signalled") { "SIGHUP through the shared PID namespace" } else { "the edge's own file watch" };
                        format!("{pod}: {via}")
                    })
                    .collect();
                return Ok(format!("ConfigMap edit reached every edge pod in {}s ({})", t0.elapsed().as_secs(), how.join("; ")));
            }
            if t0.elapsed() > Duration::from_secs(180) {
                return fail(format!("after 180s only {reloaded:?} of {} edge pods reloaded", edges.len()));
            }
            std::thread::sleep(Duration::from_secs(5));
        }
    });
}

fn teardown(t: &mut Tally, c: &mut Ctx) {
    t.run("teardown", || {
        // The claims' volumes, to look for after the claims are gone.
        for pvc in c.k.items(&format!("/api/v1/namespaces/{}/persistentvolumeclaims", c.ns)) {
            if let Some(v) = pvc["spec"]["volumeName"].as_str() {
                c.pv_names.insert(v.to_string());
            }
        }
        let mut errors = Vec::new();
        for (path, name) in c.created.iter().rev() {
            match c.k.delete(&format!("{path}/{name}")) {
                Ok((200 | 202 | 404, _)) => {}
                other => errors.push(format!("{path}/{name}: {other:?}")),
            }
        }
        // The StatefulSet's claim (from volumeClaimTemplates) is not deleted
        // with it, as upstream: delete the project's claims, as `sc delete
        // project` would.
        for pvc in c.k.items(&format!("/api/v1/namespaces/{}/persistentvolumeclaims", c.ns)) {
            if let Some(name) = pvc["metadata"]["name"].as_str() {
                let _ = c.k.delete(&format!("/api/v1/namespaces/{}/persistentvolumeclaims/{name}", c.ns));
            }
        }
        let t0 = Instant::now();
        loop {
            let pods = c.all_pods().len();
            let pvcs = c.k.items(&format!("/api/v1/namespaces/{}/persistentvolumeclaims", c.ns)).len();
            if pods == 0 && pvcs == 0 {
                break;
            }
            if t0.elapsed() > Duration::from_secs(240) {
                return fail(format!("after 240s: {pods} pods, {pvcs} claims left; {}", errors.join("; ")));
            }
            std::thread::sleep(Duration::from_secs(3));
        }
        if !errors.is_empty() {
            return fail(errors.join("; "));
        }
        Ok(format!("{} objects deleted; no pod or claim left after {}s", c.created.len(), t0.elapsed().as_secs()))
    });

    t.run("nothing-left", || {
        let t0 = Instant::now();
        loop {
            let mut left = Vec::new();
            for pv in &c.pv_names {
                match c.k.get(&format!("/api/v1/persistentvolumes/{pv}")) {
                    Ok((404, _)) => {}
                    Ok((200, _)) => left.push(format!("PersistentVolume {pv}")),
                    Ok((s, _)) => return Err(Bad::Skip(format!("cannot read PersistentVolumes ({s}): declare cluster_read"))),
                    Err(e) => return fail(e),
                }
            }
            let volumes = match c.k.get("/apis/storage.storm.io/v1/volumes") {
                Ok((200, v)) => Some(v["items"].as_array().cloned().unwrap_or_default()),
                _ => None,
            };
            if let Some(vols) = &volumes {
                for v in vols {
                    let text = v.to_string();
                    let name = v["metadata"]["name"].as_str().unwrap_or("?");
                    let hit = c.pv_names.iter().any(|p| text.contains(p.as_str()))
                        || c.container_ids.iter().any(|id| text.contains(id.as_str()))
                        || text.contains(&format!("\"namespace\":\"{}\"", c.ns));
                    if hit {
                        left.push(format!("storage.storm.io Volume {name}"));
                    }
                }
            }
            if left.is_empty() {
                return match volumes {
                    Some(v) => Ok(format!(
                        "no PersistentVolume of the project's {} claims, and none of the node's {} volumes names a claim, a container ({} seen) or the project",
                        c.pv_names.len(),
                        v.len(),
                        c.container_ids.len()
                    )),
                    None => Err(Bad::Skip(format!(
                        "no PersistentVolume left ({}); the node's volumes are not readable (storage.storm.io/v1 volumes), so clones were not checked",
                        c.pv_names.len()
                    ))),
                };
            }
            if t0.elapsed() > Duration::from_secs(180) {
                return fail(format!("left after 180s: {}", left.join(", ")));
            }
            std::thread::sleep(Duration::from_secs(5));
        }
    });
}

fn medium(t: &mut Tally, k: Kube, waves: u32) -> Result<(), String> {
    let ns = env("STORM_NAMESPACE_STOREFRONT").ok_or("STORM_NAMESPACE_STOREFRONT is not set: the suite needs extra_namespaces = [\"storefront\"]")?;
    let run = env("STORM_RUN_ID").unwrap_or_else(|| "manual".into());
    let mut c = Ctx {
        k,
        overlay: Overlay::new(&ns, &run),
        ns,
        node: env("STORM_NODE"),
        created: Vec::new(),
        container_ids: BTreeSet::new(),
        pv_names: BTreeSet::new(),
        edge_ip: None,
        lb_ip: None,
        order_id: None,
    };
    if deploy(t, &mut c) {
        checks(t, &mut c);
        if waves <= 1 {
            disruptions(t, &mut c, 0);
        } else {
            let mut times = Vec::new();
            for w in 1..=waves {
                let t0 = Instant::now();
                disruptions(t, &mut c, w);
                times.push(t0.elapsed().as_secs());
            }
            let first = times[0].max(1);
            let worst = *times.iter().max().unwrap_or(&0);
            t.run("waves", || {
                if worst > first * 2 + 30 {
                    fail(format!("wave times {times:?}s: the slowest is more than twice the first"))
                } else {
                    Ok(format!("wave times {times:?}s"))
                }
            });
        }
        config_reload(t, &mut c);
        if let Some(id) = c.order_id {
            t.run("order-still-there", || match c.edge("GET", &format!("/api/orders/{id}"), b"") {
                Ok(r) if r.status == 200 => Ok(format!("order {id} still readable after every disruption")),
                Ok(r) => fail(format!("order {id}: {}", r.status)),
                Err(Bad::Fail(e)) | Err(Bad::Skip(e)) | Err(Bad::Gap(_, e)) => fail(e),
            });
        }
    }
    write_result("pods-final.txt", &pods_table(&c.all_pods()));
    teardown(t, &mut c);
    Ok(())
}

fn main() -> ExitCode {
    let suite = std::env::args().nth(1).or_else(|| env("STORM_SUITE")).unwrap_or_else(|| "short".into());
    let mut t = Tally::default();
    t.run("validate", validate_offline);
    let infra = match Kube::from_env() {
        Err(e) => {
            t.record("apiserver", "skip", 0, &e);
            Some(e)
        }
        Ok(k) => {
            t.run("api-kinds", || api_kinds(&k));
            match suite.as_str() {
                "short" => None,
                "medium" => medium(&mut t, k, 1).err(),
                "long" => medium(&mut t, k, 5).err(),
                other => Some(format!("unknown suite {other:?} (short, medium, long)")),
            }
        }
    };
    if let Some(e) = &infra {
        eprintln!("cannot run: {e}");
    }
    println!("{}", json!({"summary": {"pass": t.pass, "fail": t.fail, "skip": t.skip}}));
    write_result("report.json", &serde_json::to_string_pretty(&t.lines).unwrap_or_default());
    if t.fail > 0 {
        ExitCode::from(1)
    } else if infra.is_some() {
        ExitCode::from(2)
    } else {
        ExitCode::SUCCESS
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metrics_are_read() {
        let text = "# TYPE x gauge\nstorefront_edge_backend_active_servers{backend=\"web\"} 6\n";
        assert_eq!(metric(text, "storefront_edge_backend_active_servers{backend=\"web\"}"), Some(6.0));
        assert_eq!(metric(text, "nope"), None);
    }

    #[test]
    fn readiness_needs_every_container() {
        let p = json!({"metadata": {}, "spec": {"containers": [{}, {}]}, "status": {"phase": "Running",
            "containerStatuses": [{"ready": true}, {"ready": false}]}});
        assert!(!pod_ready(&p));
        let p = json!({"metadata": {}, "spec": {"containers": [{}, {}]}, "status": {"phase": "Running",
            "containerStatuses": [{"ready": true}, {"ready": true}]}});
        assert!(pod_ready(&p));
    }
}
