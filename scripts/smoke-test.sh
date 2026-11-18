#!/bin/sh
# Exercises the deployed storefront from outside the cluster, through the
# LoadBalancer address and through the HTTPRoute host (stormlb on a node's
# :80), as a person would. The full proof runs in the test container
# (`stormcentral test run stormstorefront medium`); this is the by-hand one.
#
#   NODE=<node address> ./scripts/smoke-test.sh
set -eu
CLI=${CLI:-sc}
NS=${NS:-storefront}

fail=0
check() {
    label=$1; url=$2; expect=$3; shift 3
    if body=$(curl -fsS --max-time 5 "$@" "$url" 2>&1) && printf '%s' "$body" | grep -q "$expect"; then
        echo "  PASS  $label"
    else
        echo "  FAIL  $label (expected /$expect/)"
        printf '%s\n' "$body" | sed 's/^/        /' | head -5
        fail=1
    fi
}

suite() {
    base=$1; shift
    check "edge /healthz"      "$base/healthz"      "ok"                              "$@"
    check "GET /"              "$base/"             "Storefront"                      "$@"
    check "content revision"   "$base/version.json" "revision"                        "$@"
    check "GET /api/healthz"   "$base/api/healthz"  '"status": "ok"'                  "$@"
    check "GET /api/readyz"    "$base/api/readyz"   '"status": "ready"'               "$@"
    check "GET /api/products"  "$base/api/products" "ANV-001"                         "$@"
    check "api metrics"        "$base/api/metrics"  "storefront_api_requests_total"   "$@"
    check "cached read"        "$base/api/products" '"source": "cache"'               "$@"
    pods=$(for _ in 1 2 3 4 5 6 7 8 9 10; do
               curl -fsS "$@" "$base/api/products" | grep -o '"pod": *"[^"]*"' | head -1
           done | sort -u | wc -l | tr -d ' ')
    if [ "$pods" -gt 1 ]; then echo "  PASS  10 requests hit $pods API pods"; else echo "  WARN  every request hit one API pod"; fi
}

lb=$("$CLI" -n "$NS" get svc edge-lb \
     -o jsonpath='{.status.loadBalancer.ingress[0].ip}{.status.loadBalancer.ingress[0].hostname}' 2>/dev/null || true)
if [ -n "$lb" ]; then
    echo "==> through the LoadBalancer address $lb"
    suite "http://$lb"
else
    echo "==> SKIP the LoadBalancer: edge-lb has no address (no pool on this site)"
fi

host=$("$CLI" -n "$NS" get httproute storefront -o jsonpath='{.spec.hostnames[0]}')
node=${NODE:-}
if [ -n "$node" ]; then
    echo "==> through the HTTPRoute host $host on $node:80"
    suite "http://$node" -H "Host: $host"
else
    echo "==> through the HTTPRoute host $host (set NODE=<address> if it does not resolve)"
    suite "http://$host"
fi

echo
[ "$fail" -eq 0 ] && echo "all checks passed" || echo "some checks FAILED"
exit "$fail"
