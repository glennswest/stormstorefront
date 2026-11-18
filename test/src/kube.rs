//! A small apiserver client: the run's ServiceAccount token, the cluster CA,
//! blocking JSON calls. What `sc` does for a person, the test does here.

use serde_json::Value;
use std::time::Duration;

pub struct Kube {
    base: String,
    token: String,
    client: reqwest::blocking::Client,
}

const SA_DIR: &str = "/var/run/secrets/kubernetes.io/serviceaccount";

pub type Res = Result<(u16, Value), String>;

impl Kube {
    pub fn from_env() -> Result<Kube, String> {
        let base = std::env::var("STORM_API").map_err(|_| "STORM_API is not set".to_string())?;
        let token = std::fs::read_to_string(format!("{SA_DIR}/token")).map_err(|e| format!("no ServiceAccount token: {e}"))?;
        let mut b = reqwest::blocking::Client::builder().timeout(Duration::from_secs(30));
        match std::fs::read(format!("{SA_DIR}/ca.crt")) {
            Ok(pem) => {
                let cert = reqwest::Certificate::from_pem(&pem).map_err(|e| format!("ca.crt: {e}"))?;
                b = b.add_root_certificate(cert);
            }
            Err(e) => return Err(format!("no cluster CA: {e}")),
        }
        let client = b.build().map_err(|e| e.to_string())?;
        Ok(Kube { base: base.trim_end_matches('/').to_string(), token: token.trim().to_string(), client })
    }

    fn send(&self, method: reqwest::Method, path: &str, body: Option<(&str, String)>) -> Res {
        let mut req = self.client.request(method, format!("{}{path}", self.base)).bearer_auth(&self.token);
        if let Some((ctype, b)) = body {
            req = req.header("Content-Type", ctype).body(b);
        }
        let resp = req.send().map_err(|e| format!("{path}: {e}"))?;
        let status = resp.status().as_u16();
        let text = resp.text().map_err(|e| format!("{path}: {e}"))?;
        let v = serde_json::from_str(&text).unwrap_or(Value::String(text));
        Ok((status, v))
    }

    pub fn get(&self, path: &str) -> Res {
        self.send(reqwest::Method::GET, path, None)
    }

    /// The raw text of a GET (pod logs).
    pub fn get_text(&self, path: &str) -> Result<(u16, String), String> {
        let resp = self.client.get(format!("{}{path}", self.base)).bearer_auth(&self.token).send().map_err(|e| format!("{path}: {e}"))?;
        let status = resp.status().as_u16();
        Ok((status, resp.text().map_err(|e| format!("{path}: {e}"))?))
    }

    pub fn post(&self, path: &str, body: &Value) -> Res {
        self.send(reqwest::Method::POST, path, Some(("application/json", body.to_string())))
    }

    pub fn merge_patch(&self, path: &str, body: &Value) -> Res {
        self.send(reqwest::Method::PATCH, path, Some(("application/merge-patch+json", body.to_string())))
    }

    pub fn delete(&self, path: &str) -> Res {
        self.send(reqwest::Method::DELETE, path, None)
    }

    /// Items of a list call (empty on any error).
    pub fn items(&self, path: &str) -> Vec<Value> {
        match self.get(path) {
            Ok((200, v)) => v["items"].as_array().cloned().unwrap_or_default(),
            _ => Vec::new(),
        }
    }
}

/// The REST collection of a kind in a namespace (None: a kind we do not make).
pub fn collection(api_version: &str, kind: &str, ns: &str) -> Option<String> {
    let plural = match kind {
        "ResourceQuota" => "resourcequotas",
        "LimitRange" => "limitranges",
        "ServiceAccount" => "serviceaccounts",
        "Secret" => "secrets",
        "ConfigMap" => "configmaps",
        "Service" => "services",
        "PersistentVolumeClaim" => "persistentvolumeclaims",
        "Pod" => "pods",
        "Deployment" => "deployments",
        "StatefulSet" => "statefulsets",
        "Role" => "roles",
        "RoleBinding" => "rolebindings",
        "NetworkPolicy" => "networkpolicies",
        "HorizontalPodAutoscaler" => "horizontalpodautoscalers",
        "PodDisruptionBudget" => "poddisruptionbudgets",
        "HTTPRoute" => "httproutes",
        _ => return None,
    };
    Some(if api_version == "v1" {
        format!("/api/v1/namespaces/{ns}/{plural}")
    } else {
        format!("/apis/{api_version}/namespaces/{ns}/{plural}")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collections() {
        assert_eq!(collection("v1", "Secret", "x").unwrap(), "/api/v1/namespaces/x/secrets");
        assert_eq!(collection("apps/v1", "StatefulSet", "x").unwrap(), "/apis/apps/v1/namespaces/x/statefulsets");
        assert!(collection("v1", "Namespace", "x").is_none());
    }
}
