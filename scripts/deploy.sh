#!/bin/sh
# Applies the storefront in dependency order with sc and waits for each tier.
#
#   ./scripts/deploy.sh              # sc, project storefront
#   CLI=kubectl ./scripts/deploy.sh  # any Kubernetes client
#
# `sc apply -k manifests/` applies everything at once; this does the same in
# the order the tiers depend on each other, as the original's deploy.sh.
set -eu
root=$(cd "$(dirname "$0")/.." && pwd)
CLI=${CLI:-sc}
NS=${NS:-storefront}
command -v "$CLI" >/dev/null || { echo "$CLI not found on PATH" >&2; exit 1; }

# Real credentials or nothing: the committed 03-secrets.yaml holds public
# CHANGEME placeholders and is never applied. Either the generated file
# (`make secrets`) or Secrets already in the project (made by hand or by the
# cluster's secret store, kept as they are).
local_secrets="$root/manifests/03-secrets.local.yaml"
if [ -f "$local_secrets" ]; then
    if grep -q CHANGEME "$local_secrets"; then
        echo "$local_secrets still holds CHANGEME placeholders; run 'FORCE=1 make secrets'" >&2
        exit 1
    fi
    echo "==> using generated secrets"
elif "$CLI" -n "$NS" get secret storefront-db >/dev/null 2>&1 \
     && "$CLI" -n "$NS" get secret storefront-cache >/dev/null 2>&1; then
    local_secrets=
    echo "==> using the project's existing secrets"
else
    echo "no credentials: run 'make secrets' (or create storefront-db and storefront-cache in $NS)" >&2
    exit 1
fi

apply() { echo "==> apply $(basename "$1")"; "$CLI" apply -f "$1"; }

apply "$root/manifests/00-namespace.yaml"
apply "$root/manifests/01-quota-limits.yaml"
apply "$root/manifests/02-rbac.yaml"
[ -n "$local_secrets" ] && apply "$local_secrets"
for f in 04-config-edge 05-config-web 06-config-api; do
    apply "$root/manifests/$f.yaml"
done

# Data tier first: the API's init container blocks on it.
apply "$root/manifests/10-data-postgres.yaml"
apply "$root/manifests/11-data-cache.yaml"
echo "==> waiting for the data tier"
"$CLI" -n "$NS" rollout status statefulset/postgres --timeout=300s
"$CLI" -n "$NS" rollout status deployment/cache --timeout=180s

apply "$root/manifests/20-api.yaml"
"$CLI" -n "$NS" rollout status deployment/api --timeout=300s

apply "$root/manifests/30-web.yaml"
"$CLI" -n "$NS" rollout status deployment/web --timeout=300s

apply "$root/manifests/40-edge.yaml"
"$CLI" -n "$NS" rollout status deployment/edge --timeout=300s

apply "$root/manifests/50-networkpolicy.yaml"
apply "$root/manifests/60-autoscale-pdb.yaml"
apply "$root/manifests/70-monitoring.yaml"

echo
echo "==> done"
"$CLI" -n "$NS" get pods -o wide
echo
host=$("$CLI" -n "$NS" get httproute storefront -o jsonpath='{.spec.hostnames[0]}' 2>/dev/null || true)
[ -n "$host" ] && echo "HTTPRoute:    http://$host/  (stormlb on any node's :80)"
lb=$("$CLI" -n "$NS" get svc edge-lb \
     -o jsonpath='{.status.loadBalancer.ingress[0].ip}{.status.loadBalancer.ingress[0].hostname}' 2>/dev/null || true)
if [ -n "$lb" ]; then
    echo "LoadBalancer: http://$lb/"
else
    echo "LoadBalancer: <pending> -- no pool on this site? Use the HTTPRoute, or:"
    echo "              $CLI -n $NS port-forward svc/edge-lb 8080:80"
fi
