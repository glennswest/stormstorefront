#!/bin/sh
# Build stormstorefront's test image context for the commit checked out.
#
#   test/build.sh [target]        default x86_64-unknown-linux-musl
#
# Per stormcentral docs/test-standard.md this runs first, in the checkout on
# the build box, with cargo: it builds the static test binary and stages it in
# test/.stage/, which test/Containerfile (context: the repo root) packages
# FROM scratch. One image serves all three suites (`/test <suite>`).
set -eu
target=${1:-x86_64-unknown-linux-musl}
root=$(cd "$(dirname "$0")/.." && pwd)

if [ -f "$root/Cargo.lock" ]; then locked=--locked; else locked=; fi
cargo build --release $locked --target "$target" -p stormstorefront-test --manifest-path "$root/Cargo.toml"
# cargo's target dir is $CARGO_TARGET_DIR on the build box, not ./target:
# ask cargo where it is.
tdir=$(cargo metadata --format-version 1 --no-deps --manifest-path "$root/Cargo.toml" |
    sed 's/.*"target_directory":"\([^"]*\)".*/\1/')
stage="$root/test/.stage"
rm -rf "$stage"
mkdir -p "$stage"
cp "$tdir/$target/release/stormstorefront-test" "$stage/"
echo "$stage"
