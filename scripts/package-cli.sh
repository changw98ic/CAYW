#!/bin/sh
# Assemble a local macOS arm64 product bundle. Does not publish or install.
set -eu
[ "${1:-}" = --help ] && { echo 'Usage: sh scripts/package-cli.sh OUTPUT_DIRECTORY'; exit 0; }
[ $# -eq 1 ] || { echo 'expected a new output directory' >&2; exit 1; }
[ "$(uname -s)" = Darwin ] && [ "$(uname -m)" = arm64 ] || { echo 'packaging requires macOS arm64; this platform is unverified' >&2; exit 1; }
root=$(CDPATH= cd -P "$(dirname "$0")/.." && pwd)
cd "$root"
[ -z "$(git status --porcelain)" ] || { echo "commit or stash source changes before packaging; BUILD.json binds a clean commit" >&2; exit 1; }
case $1 in /*) output=$1 ;; *) output=$root/$1 ;; esac
[ ! -e "$output" ] || { echo 'output already exists; refusing to replace it' >&2; exit 1; }
parent=$(dirname "$output")
mkdir -p "$parent"
stage=$(mktemp -d "$parent/.cayw-package.XXXXXX")
trap 'rm -rf "$stage"' 0
mkdir "$stage/bin"
cargo build --release --locked -p cayw-runtime --bin cayw --features plugins
cargo build --release --locked -p cayw-tui --bin cayw-tui -p cayw-webui --bin cayw-webui
cargo build --release --locked --manifest-path examples/headless-client/Cargo.toml --bin cayw-client
for program in cayw cayw-tui cayw-webui; do
    cp "${CARGO_TARGET_DIR:-target}/release/$program" "$stage/bin/$program"
done
cp "${CARGO_TARGET_DIR:-examples/headless-client/target}/release/cayw-client" "$stage/bin/cayw-client"
cp scripts/install-cli.sh "$stage/install.sh"
cp LICENSE "$stage/LICENSE"
revision=$(git rev-parse HEAD)
printf '{"source_commit":"%s","target":"aarch64-apple-darwin","runtime_features":["plugins"],"version":"0.1.0"}\n' "$revision" > "$stage/BUILD.json"
(cd "$stage" && shasum -a 256 bin/cayw bin/cayw-client bin/cayw-tui bin/cayw-webui install.sh BUILD.json LICENSE > SHA256SUMS)
for program in cayw cayw-client cayw-tui cayw-webui; do "$stage/bin/$program" --help >/dev/null; done
mv "$stage" "$output"
printf 'Bundle assembled: %s\n' "$output"
