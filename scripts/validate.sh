#!/bin/sh
# Offline checks of the manifests: no cluster required. The original's
# scripts/validate.sh, now `stormstorefront validate` (src/validate.rs): YAML,
# the embedded configs, restricted-v2 hygiene, offline-and-ours images.
#
#   scripts/validate.sh [manifests-dir]
#
# Uses the built binary when there is one ($STOREFRONT), else cargo.
set -eu
root=$(cd "$(dirname "$0")/.." && pwd)
dir=${1:-$root/manifests}
if [ -n "${STOREFRONT:-}" ]; then
    exec "$STOREFRONT" validate "$dir"
fi
exec cargo run --quiet --manifest-path "$root/Cargo.toml" --bin stormstorefront -- validate "$dir"
