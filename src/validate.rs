//! Offline checks of the manifests: no cluster required. The original's
//! `scripts/validate.sh`, in Rust, plus the rules stormcos adds.
//!
//! - every manifest is well-formed YAML, and `kustomization.yaml` lists
//!   every one of them (and nothing else);
//! - every embedded config parses: the edge's `edge.toml` (as `haproxy -c`
//!   would check the original's), the store's `seed.json`; a `*.sh` key is an
//!   error, since the image has no shell;
//! - every workload admits as `restricted-v2` would have it: `runAsNonRoot`,
//!   `seccompProfile: RuntimeDefault`, no `runAsUser`/`fsGroup`, every
//!   container `allowPrivilegeEscalation: false`, `drop: [ALL]`, nothing
//!   added, not privileged, requests and limits, no port under 1024, no host
//!   namespaces or `hostPath`;
//! - **offline and ours**: every image is one of ours by name (resolved by the
//!   node from its release or its registry, never a registry host), and every
//!   container says its `command` (a registry image carries no entrypoint),
//!   which for our image is `/bin/stormstorefront <role>` with a known role;
//! - no `subPath` (it would never see a ConfigMap update);
//! - Secrets hold placeholders only (`CHANGEME…`): real ones are made by
//!   `scripts/make-secrets.sh` into a gitignored file.

use serde_yaml::Value;
use std::collections::BTreeSet;

/// The binary's path inside the image (a `binaries` image: `bin/<name>`).
pub const BINARY: &str = "/bin/stormstorefront";

/// Images a manifest may name: ours, and the release's own components.
pub const IMAGES: [&str; 3] = ["stormstorefront", "fastetcd", "stormlb"];

#[derive(Debug, Default)]
pub struct Report {
    pub objects: usize,
    pub configs: usize,
    pub pods: u64,
    pub containers: u64,
    pub init_containers: u64,
    pub errors: Vec<String>,
    pub warnings: Vec<String>,
}

impl Report {
    pub fn ok(&self) -> bool {
        self.errors.is_empty()
    }

    pub fn render(&self) -> String {
        let mut out = format!(
            "checked {} objects and {} embedded configs: {} pods, {} running containers, {} initContainers at steady state\n",
            self.objects, self.configs, self.pods, self.containers, self.init_containers
        );
        for w in &self.warnings {
            out.push_str(&format!("  warn:  {w}\n"));
        }
        for e in &self.errors {
            out.push_str(&format!("  ERROR: {e}\n"));
        }
        out
    }
}

fn s(v: &Value) -> &str {
    v.as_str().unwrap_or("")
}

fn empty_map(v: &Value) -> bool {
    match v.as_mapping() {
        Some(m) => m.is_empty(),
        None => true,
    }
}

fn seq(v: &Value) -> &[Value] {
    v.as_sequence().map(|x| x.as_slice()).unwrap_or(&[])
}

/// The name of the image, without a tag or digest.
pub fn image_name(image: &str) -> &str {
    let no_digest = image.split('@').next().unwrap_or(image);
    match no_digest.rsplit_once(':') {
        Some((n, tag)) if !tag.contains('/') => n,
        _ => no_digest,
    }
}

fn check_container(where_: &str, c: &Value, roles: &[&str], is_init: bool, r: &mut Report) {
    let sc = &c["securityContext"];
    if sc["allowPrivilegeEscalation"].as_bool() != Some(false) {
        r.errors.push(format!("{where_}: allowPrivilegeEscalation must be false"));
    }
    let drop: Vec<&str> = seq(&sc["capabilities"]["drop"]).iter().map(s).collect();
    if drop != ["ALL"] {
        r.errors.push(format!("{where_}: capabilities must drop ALL"));
    }
    if !seq(&sc["capabilities"]["add"]).is_empty() {
        r.errors.push(format!("{where_}: adds capabilities; restricted-v2 allows none"));
    }
    if sc["privileged"].as_bool() == Some(true) {
        r.errors.push(format!("{where_}: privileged is not allowed"));
    }
    if !sc["runAsUser"].is_null() || !sc["runAsGroup"].is_null() {
        r.errors.push(format!("{where_}: container pins runAsUser/runAsGroup; restricted-v2 assigns them"));
    }
    if sc["readOnlyRootFilesystem"].as_bool() != Some(true) {
        r.warnings.push(format!("{where_}: readOnlyRootFilesystem is not true"));
    }
    let res = &c["resources"];
    if empty_map(&res["requests"]) || empty_map(&res["limits"]) {
        r.errors.push(format!("{where_}: missing resource requests or limits"));
    }
    for p in seq(&c["ports"]) {
        let port = p["containerPort"].as_u64().unwrap_or(0);
        if port < 1024 {
            r.errors.push(format!("{where_}: port {port} is privileged; an arbitrary UID cannot bind it"));
        }
    }
    for m in seq(&c["volumeMounts"]) {
        if !m["subPath"].is_null() || !m["subPathExpr"].is_null() {
            r.errors.push(format!("{where_}: mounts {} with subPath, which never sees a ConfigMap update", s(&m["name"])));
        }
    }
    let image = s(&c["image"]);
    let name = image_name(image);
    if !IMAGES.contains(&name) {
        r.errors.push(format!(
            "{where_}: image {image:?} is not ours; every image comes from the node's release or registry ({})",
            IMAGES.join(", ")
        ));
    }
    let command: Vec<&str> = seq(&c["command"]).iter().map(s).collect();
    if command.is_empty() {
        r.errors.push(format!("{where_}: no command; a registry image carries no entrypoint"));
    } else if name == "stormstorefront" {
        if command[0] != BINARY {
            r.errors.push(format!("{where_}: command[0] is {:?}, not {BINARY}", command[0]));
        } else {
            match command.get(1) {
                Some(role) if roles.contains(role) => {}
                other => r.errors.push(format!("{where_}: unknown role {other:?} (roles: {})", roles.join(" "))),
            }
        }
    }
    // Probes and hooks run in the container: no shell, so only our binary.
    let mut execs: Vec<(&str, &Value)> = Vec::new();
    for probe in ["startupProbe", "readinessProbe", "livenessProbe"] {
        execs.push((probe, &c[probe]["exec"]["command"]));
    }
    execs.push(("preStop", &c["lifecycle"]["preStop"]["exec"]["command"]));
    execs.push(("postStart", &c["lifecycle"]["postStart"]["exec"]["command"]));
    for (what, cmd) in execs {
        if let Some(first) = seq(cmd).first() {
            if s(first) != BINARY {
                r.errors.push(format!("{where_}: {what} runs {:?}; the image has only {BINARY}", s(first)));
            }
        }
    }
    // A sidecar with no port has nothing to be ready for; one that serves does.
    if !is_init && !seq(&c["ports"]).is_empty() && c["readinessProbe"].is_null() {
        r.warnings.push(format!("{where_}: serves a port with no readinessProbe"));
    }
}

fn check_workload(doc: &Value, roles: &[&str], r: &mut Report) {
    let kind = s(&doc["kind"]);
    let name = s(&doc["metadata"]["name"]);
    let spec = &doc["spec"]["template"]["spec"];
    let psc = &spec["securityContext"];
    if !psc["runAsUser"].is_null() {
        r.errors.push(format!("{name}: pod sets runAsUser; restricted-v2 assigns the UID"));
    }
    if !psc["runAsGroup"].is_null() {
        r.errors.push(format!("{name}: pod sets runAsGroup; restricted-v2 assigns it"));
    }
    if !psc["fsGroup"].is_null() {
        r.errors.push(format!("{name}: pod sets fsGroup; restricted-v2 assigns it"));
    }
    if s(&psc["seccompProfile"]["type"]) != "RuntimeDefault" {
        r.errors.push(format!("{name}: pod is missing seccompProfile RuntimeDefault"));
    }
    if psc["runAsNonRoot"].as_bool() != Some(true) {
        r.errors.push(format!("{name}: pod is missing runAsNonRoot: true"));
    }
    for host in ["hostNetwork", "hostPID", "hostIPC"] {
        if spec[host].as_bool() == Some(true) {
            r.errors.push(format!("{name}: {host} is not allowed"));
        }
    }
    for v in seq(&spec["volumes"]) {
        if !v["hostPath"].is_null() {
            r.errors.push(format!("{name}: volume {} is a hostPath", s(&v["name"])));
        }
    }
    let replicas = match kind {
        "Deployment" | "StatefulSet" => doc["spec"]["replicas"].as_u64().unwrap_or(1),
        _ => 0,
    };
    let inits = seq(&spec["initContainers"]);
    let mains = seq(&spec["containers"]);
    if mains.is_empty() {
        r.errors.push(format!("{name}: no containers"));
    }
    if replicas > 0 && mains.len() < 2 {
        r.errors.push(format!("{name}: one container; every pod here runs more than one"));
    }
    r.pods += replicas;
    r.containers += replicas * mains.len() as u64;
    r.init_containers += replicas * inits.len() as u64;
    let mut names = BTreeSet::new();
    for (c, is_init) in inits.iter().map(|c| (c, true)).chain(mains.iter().map(|c| (c, false))) {
        let cname = s(&c["name"]);
        if !names.insert(cname.to_string()) {
            r.errors.push(format!("{name}: two containers named {cname}"));
        }
        check_container(&format!("{name}/{cname}"), c, roles, is_init, r);
    }
}

fn check_configmap(doc: &Value, r: &mut Report) {
    let name = s(&doc["metadata"]["name"]);
    let Some(data) = doc["data"].as_mapping() else { return };
    for (k, v) in data {
        let key = s(k);
        let body = s(v);
        if key.ends_with(".sh") {
            r.errors.push(format!("{name}/{key}: a shell script; the image has no shell, every role is Rust"));
        } else if key.ends_with("edge.toml") {
            r.configs += 1;
            if let Err(e) = crate::edge::Config::parse(body) {
                r.errors.push(format!("{name}/{key}: {e}"));
            }
        } else if key.ends_with(".yml") {
            r.configs += 1;
            if let Err(e) = serde_yaml::from_str::<Value>(body) {
                r.errors.push(format!("{name}/{key}: not YAML: {e}"));
            }
        } else if key == "seed.json" {
            r.configs += 1;
            match serde_json::from_str::<serde_json::Value>(body) {
                Ok(seed) => {
                    if let Err(e) = crate::store::check_seed(&seed) {
                        r.errors.push(format!("{name}/{key}: {e}"));
                    }
                }
                Err(e) => r.errors.push(format!("{name}/{key}: not JSON: {e}")),
            }
        }
    }
}

fn check_secret(doc: &Value, r: &mut Report) {
    let name = s(&doc["metadata"]["name"]);
    if !doc["data"].is_null() {
        r.errors.push(format!("{name}: Secret with base64 data; committed Secrets hold stringData placeholders only"));
    }
    if let Some(data) = doc["stringData"].as_mapping() {
        for (k, v) in data {
            if !s(v).contains("CHANGEME") {
                r.errors.push(format!("{name}/{}: not a CHANGEME placeholder; real secrets never go in git", s(k)));
            }
        }
    }
}

/// Validates `files` — (file name, contents) of every manifest, the
/// kustomization among them — against `roles` (the binary's roles).
pub fn validate(files: &[(String, String)], roles: &[&str]) -> Report {
    let mut r = Report::default();
    let mut listed: Option<BTreeSet<String>> = None;
    let mut present = BTreeSet::new();
    for (file, text) in files {
        if file == "kustomization.yaml" {
            match serde_yaml::from_str::<Value>(text) {
                Ok(k) => listed = Some(seq(&k["resources"]).iter().map(|v| s(v).to_string()).collect()),
                Err(e) => r.errors.push(format!("{file}: unparseable YAML: {e}")),
            }
            continue;
        }
        present.insert(file.clone());
        for doc in serde_yaml::Deserializer::from_str(text) {
            let doc = match <Value as serde::Deserialize>::deserialize(doc) {
                Ok(d) => d,
                Err(e) => {
                    r.errors.push(format!("{file}: unparseable YAML: {e}"));
                    break;
                }
            };
            if doc.is_null() {
                continue;
            }
            r.objects += 1;
            let kind = s(&doc["kind"]);
            let name = s(&doc["metadata"]["name"]);
            if kind.is_empty() || name.is_empty() {
                r.errors.push(format!("{file}: an object without kind or metadata.name"));
                continue;
            }
            let ns = s(&doc["metadata"]["namespace"]);
            if kind != "Namespace" && !ns.is_empty() && ns != "storefront" {
                r.errors.push(format!("{file}: {kind}/{name} is in namespace {ns}, not storefront"));
            }
            match kind {
                "Deployment" | "StatefulSet" | "DaemonSet" | "Job" => check_workload(&doc, roles, &mut r),
                "ConfigMap" => check_configmap(&doc, &mut r),
                "Secret" => check_secret(&doc, &mut r),
                "Namespace" => {
                    if s(&doc["metadata"]["labels"]["pod-security.kubernetes.io/enforce"]) != "restricted" {
                        r.errors.push(format!("{file}: namespace {name} does not enforce pod-security restricted"));
                    }
                }
                _ => {}
            }
        }
    }
    match listed {
        None => r.errors.push("no kustomization.yaml".into()),
        Some(listed) => {
            for f in present.difference(&listed) {
                if f != "03-secrets.local.yaml" {
                    r.errors.push(format!("kustomization.yaml does not list {f}"));
                }
            }
            for f in listed.difference(&present) {
                r.errors.push(format!("kustomization.yaml lists {f}, which does not exist"));
            }
        }
    }
    r
}

/// Reads every `*.yaml` in `dir` and validates them.
pub fn validate_dir(dir: &std::path::Path, roles: &[&str]) -> std::io::Result<Report> {
    let mut files = Vec::new();
    for e in std::fs::read_dir(dir)? {
        let p = e?.path();
        if p.extension().is_some_and(|x| x == "yaml") {
            let name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            files.push((name, std::fs::read_to_string(&p)?));
        }
    }
    files.sort();
    Ok(validate(&files, roles))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_names() {
        assert_eq!(image_name("stormstorefront"), "stormstorefront");
        assert_eq!(image_name("stormstorefront:abc123"), "stormstorefront");
        assert_eq!(image_name("quay.io/x/y:1"), "quay.io/x/y");
        assert_eq!(image_name("localhost:5000/x"), "localhost:5000/x");
        assert_eq!(image_name("a@sha256:00"), "a");
    }

    #[test]
    fn the_repos_manifests_pass() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("manifests");
        let r = validate_dir(&dir, crate::ROLES).unwrap();
        assert!(r.ok(), "{}", r.render());
        assert!(r.warnings.is_empty(), "{}", r.render());
        assert_eq!((r.pods, r.containers, r.init_containers), (10, 27, 8), "{}", r.render());
    }

    #[test]
    fn catches_what_restricted_refuses() {
        let bad = r#"
apiVersion: apps/v1
kind: Deployment
metadata: {name: bad, namespace: storefront}
spec:
  replicas: 1
  template:
    spec:
      securityContext: {runAsUser: 0, fsGroup: 1}
      volumes: [{name: h, hostPath: {path: /}}]
      containers:
        - name: a
          image: docker.io/library/nginx:1
          ports: [{containerPort: 80}]
          volumeMounts: [{name: h, mountPath: /x, subPath: y}]
        - name: b
          image: stormstorefront
          command: [/bin/stormstorefront, nope]
          readinessProbe: {exec: {command: [/bin/sh, -c, "true"]}}
          securityContext: {allowPrivilegeEscalation: false, capabilities: {drop: [ALL], add: [NET_ADMIN]}, readOnlyRootFilesystem: true}
          resources: {requests: {cpu: 1m}, limits: {cpu: 1m}}
---
apiVersion: v1
kind: Secret
metadata: {name: s, namespace: storefront}
stringData: {password: hunter2}
---
apiVersion: v1
kind: ConfigMap
metadata: {name: c, namespace: storefront}
data: {run.sh: "echo", edge.toml: "listen = 1"}
"#;
        let files = vec![
            ("bad.yaml".to_string(), bad.to_string()),
            ("kustomization.yaml".to_string(), "resources: [bad.yaml, gone.yaml]\n".to_string()),
        ];
        let r = validate(&files, crate::ROLES);
        let all = r.render();
        for needle in [
            "runAsUser",
            "fsGroup",
            "seccompProfile",
            "hostPath",
            "port 80 is privileged",
            "subPath",
            "is not ours",
            "no command",
            "unknown role",
            "adds capabilities",
            "/bin/sh",
            "CHANGEME",
            "shell script",
            "edge.toml",
            "lists gone.yaml",
            "allowPrivilegeEscalation",
        ] {
            assert!(all.contains(needle), "missing {needle:?} in\n{all}");
        }
    }
}
