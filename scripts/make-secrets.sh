#!/bin/sh
# Writes manifests/03-secrets.local.yaml (gitignored) with random values in
# place of the committed placeholders. It is the file kustomization.yaml
# lists, so `sc apply -k manifests/` and scripts/deploy.sh apply it; without
# it they fail rather than deploy the public CHANGEME values.
#
# It never overwrites an existing file: re-running must not rotate the
# passwords under a running project. FORCE=1 does, on purpose.
set -eu
root=$(cd "$(dirname "$0")/.." && pwd)
out="$root/manifests/03-secrets.local.yaml"
if [ -e "$out" ] && [ "${FORCE:-}" != 1 ]; then
    echo "$out exists; kept (FORCE=1 to replace it with new values)"
    exit 0
fi
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
