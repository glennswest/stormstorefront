//! The repo's manifests, compiled in, and the test's overlay of them: the
//! same objects `sc apply -k manifests/` makes, moved into the run's project.

use serde_json::{json, Value};

/// Every manifest, as kustomization.yaml lists them (validate checks it).
pub const FILES: &[(&str, &str)] = &[
    ("00-namespace.yaml", include_str!("../../manifests/00-namespace.yaml")),
    ("01-quota-limits.yaml", include_str!("../../manifests/01-quota-limits.yaml")),
    ("02-rbac.yaml", include_str!("../../manifests/02-rbac.yaml")),
    ("03-secrets.yaml", include_str!("../../manifests/03-secrets.yaml")),
    ("04-config-edge.yaml", include_str!("../../manifests/04-config-edge.yaml")),
    ("05-config-web.yaml", include_str!("../../manifests/05-config-web.yaml")),
    ("06-config-api.yaml", include_str!("../../manifests/06-config-api.yaml")),
    ("10-data-postgres.yaml", include_str!("../../manifests/10-data-postgres.yaml")),
    ("11-data-cache.yaml", include_str!("../../manifests/11-data-cache.yaml")),
    ("20-api.yaml", include_str!("../../manifests/20-api.yaml")),
    ("30-web.yaml", include_str!("../../manifests/30-web.yaml")),
    ("40-edge.yaml", include_str!("../../manifests/40-edge.yaml")),
    ("50-networkpolicy.yaml", include_str!("../../manifests/50-networkpolicy.yaml")),
    ("60-autoscale-pdb.yaml", include_str!("../../manifests/60-autoscale-pdb.yaml")),
    ("70-monitoring.yaml", include_str!("../../manifests/70-monitoring.yaml")),
    ("kustomization.yaml", include_str!("../../manifests/kustomization.yaml")),
];

/// The objects of one file, as JSON.
pub fn objects(file: &str) -> Result<Vec<Value>, String> {
    let (_, text) = FILES.iter().find(|(f, _)| *f == file).ok_or_else(|| format!("no manifest {file}"))?;
    let mut out = Vec::new();
    for doc in serde_yaml::Deserializer::from_str(text) {
        let v: Value = <Value as serde::Deserialize>::deserialize(doc).map_err(|e| format!("{file}: {e}"))?;
        if !v.is_null() {
            out.push(v);
        }
    }
    Ok(out)
}

/// The run's settings for the overlay.
pub struct Overlay {
    pub namespace: String,
    pub run_id: String,
    pub hostname: String,
    pub secret_seed: String,
}

fn random_hex(n: usize) -> String {
    use std::io::Read;
    let mut buf = vec![0u8; n];
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        let _ = f.read_exact(&mut buf);
    }
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

impl Overlay {
    pub fn new(namespace: &str, run_id: &str) -> Overlay {
        Overlay {
            namespace: namespace.to_string(),
            run_id: run_id.to_string(),
            hostname: format!("storefront-{}.test.storm", run_id.to_ascii_lowercase()),
            secret_seed: random_hex(16),
        }
    }

    /// The object as the test makes it. `None`: not made by the test (the
    /// Namespace — the runner made the project, `extra_namespaces`).
    ///
    /// - every object in the run's project, labelled `storm.io/test-run`;
    /// - Secrets get per-run random values in place of the placeholders;
    /// - the HTTPRoute's hostname is the run's own, so runs never collide;
    /// - each NetworkPolicy rule that admits "the monitoring stack" also
    ///   admits namespaces labelled with this run (the test pod's own and
    ///   this project), so the test can read every container's metrics.
    ///   Nothing else is opened.
    pub fn apply(&self, mut obj: Value) -> Option<Value> {
        let kind = obj["kind"].as_str().unwrap_or("").to_string();
        if kind == "Namespace" {
            return None;
        }
        let meta = obj["metadata"].as_object_mut()?;
        meta.insert("namespace".into(), json!(self.namespace));
        let labels = meta.entry("labels").or_insert_with(|| json!({}));
        if let Some(l) = labels.as_object_mut() {
            l.insert("storm.io/test-run".into(), json!(self.run_id));
        }
        match kind.as_str() {
            "Secret" => {
                if let Some(data) = obj["stringData"].as_object_mut() {
                    for (k, v) in data.iter_mut() {
                        *v = json!(format!("{k}-{}", self.secret_seed));
                    }
                }
            }
            "RoleBinding" => {
                if let Some(subjects) = obj["subjects"].as_array_mut() {
                    for s in subjects {
                        s["namespace"] = json!(self.namespace);
                    }
                }
            }
            "HTTPRoute" => {
                obj["spec"]["hostnames"] = json!([self.hostname]);
            }
            "NetworkPolicy" => {
                let run_ns = json!({"namespaceSelector": {"matchLabels": {"storm.io/test-run": self.run_id}}});
                if let Some(rules) = obj["spec"]["ingress"].as_array_mut() {
                    for rule in rules {
                        let admits_monitoring = rule["from"]
                            .as_array()
                            .is_some_and(|f| f.iter().any(|p| p["namespaceSelector"]["matchLabels"]["storm.io/policy-group"] == "monitoring"));
                        if admits_monitoring {
                            if let Some(from) = rule["from"].as_array_mut() {
                                from.push(run_ns.clone());
                            }
                        }
                    }
                }
            }
            _ => {}
        }
        Some(obj)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_file_parses_and_validates() {
        let files: Vec<(String, String)> = FILES.iter().map(|(f, t)| (f.to_string(), t.to_string())).collect();
        let r = storefront::validate::validate(&files, storefront::ROLES);
        assert!(r.ok(), "{}", r.render());
        for (f, _) in FILES {
            if *f != "kustomization.yaml" {
                assert!(!objects(f).unwrap().is_empty(), "{f}");
            }
        }
    }

    #[test]
    fn the_overlay_moves_and_opens_only_metrics() {
        let o = Overlay::new("run-ns", "abc");
        assert!(o.apply(objects("00-namespace.yaml").unwrap().remove(0)).is_none());
        let secret = o.apply(objects("03-secrets.yaml").unwrap().remove(0)).unwrap();
        assert_eq!(secret["metadata"]["namespace"], "run-ns");
        assert!(!secret.to_string().contains("CHANGEME"));
        let pols: Vec<Value> = objects("50-networkpolicy.yaml").unwrap().into_iter().filter_map(|p| o.apply(p)).collect();
        let edge = pols.iter().find(|p| p["metadata"]["name"] == "edge-allow-ingress").unwrap();
        // The open 8080 rule is untouched; the metrics rule gains the run.
        assert!(edge["spec"]["ingress"][0]["from"].is_null());
        assert_eq!(edge["spec"]["ingress"][1]["from"].as_array().unwrap().len(), 2);
        let route = o.apply(objects("40-edge.yaml").unwrap().into_iter().find(|x| x["kind"] == "HTTPRoute").unwrap()).unwrap();
        assert_eq!(route["spec"]["hostnames"][0], "storefront-abc.test.storm");
    }
}
