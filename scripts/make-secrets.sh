#!/bin/sh
# Writes manifests/03-secrets.local.yaml (gitignored) with random values in
# place of the committed placeholders. scripts/deploy.sh prefers it.
set -eu
root=$(cd "$(dirname "$0")/.." && pwd)
out="$root/manifests/03-secrets.local.yaml"
rand() { od -An -tx1 -N24 /dev/urandom | tr -d ' \n'; }
umask 077
cat > "$out" <<YAML
---
apiVersion: v1
kind: Secret
metadata:
  name: storefront-db
  namespace: storefront
  labels:
    app.kubernetes.io/part-of: storefront
    app.kubernetes.io/component: data
type: Opaque
stringData:
  database-name: storefront
  database-user: storefront
  database-password: $(rand)
---
apiVersion: v1
kind: Secret
metadata:
  name: storefront-cache
  namespace: storefront
  labels:
    app.kubernetes.io/part-of: storefront
    app.kubernetes.io/component: data
type: Opaque
stringData:
  redis-password: $(rand)
YAML
echo "wrote $out (mode 600, gitignored)"
