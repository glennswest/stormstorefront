#!/bin/sh
# The full check, on a build VM: `sc-build deploy/check.sh`.
#   - the app and the test crate build and their tests pass;
#   - zero warnings: clippy -D warnings over both (owner, stormcentral#193);
#   - the manifests validate (scripts/validate.sh);
#   - the release binary builds for musl, as the image build does.
set -eu
cd "$(dirname "$0")/.."
if [ -f Cargo.lock ]; then locked=--locked; else locked=; fi
cargo build $locked --workspace --all-targets
cargo test $locked --workspace
cargo clippy $locked --workspace --all-targets -- -D warnings
./scripts/validate.sh
if rustup target list --installed 2>/dev/null | grep -q x86_64-unknown-linux-musl; then
    cargo build $locked --release --target x86_64-unknown-linux-musl -p stormstorefront -p stormstorefront-test
fi
echo "check: ok"
