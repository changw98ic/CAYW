#!/bin/sh
# Assemble a local macOS arm64 product bundle. Does not publish or install.
set -eu
[ "${1:-}" = --help ] && { echo 'Usage: sh scripts/package-cli.sh OUTPUT_DIRECTORY'; exit 0; }
[ $# -eq 1 ] || { echo 'expected a new output directory' >&2; exit 1; }
[ "$(uname -s)" = Darwin ] && [ "$(uname -m)" = arm64 ] || { echo 'packaging requires macOS arm64; this platform is unverified' >&2; exit 1; }
command -v python3 >/dev/null 2>&1 || { echo 'packaging requires python3 for third-party notices' >&2; exit 1; }
root=$(CDPATH= cd -P "$(dirname "$0")/.." && pwd)
cd "$root"
[ -z "$(git status --porcelain)" ] || { echo "commit or stash source changes before packaging; BUILD.json binds a clean commit" >&2; exit 1; }
case $1 in /*) output=$1 ;; *) output=$root/$1 ;; esac
[ ! -e "$output" ] || { echo 'output already exists; refusing to replace it' >&2; exit 1; }
revision=$(git rev-parse HEAD)
# Fix the target rather than silently honoring a cross-build CARGO_BUILD_TARGET.
target=aarch64-apple-darwin
# A shared explicit target directory also works for the independent client workspace.
case ${CARGO_TARGET_DIR:-target} in
    /*) target_dir=${CARGO_TARGET_DIR} ;;
    *) target_dir=$root/${CARGO_TARGET_DIR:-target} ;;
esac
parent=$(dirname "$output")
mkdir -p "$parent"
stage=$(mktemp -d "$parent/.cayw-package.XXXXXX")
trap 'rm -rf "$stage"' 0
mkdir "$stage/bin"
python3 scripts/generate-third-party-notices.py --root "$root" --output "$stage/THIRD-PARTY-NOTICES"
[ -s "$stage/THIRD-PARTY-NOTICES" ] || { echo 'third-party notices are empty' >&2; exit 1; }
cargo build --release --locked --target "$target" --target-dir "$target_dir" -p cayw-runtime --bin cayw --no-default-features --features plugins
cargo build --release --locked --target "$target" --target-dir "$target_dir" -p cayw-tui --bin cayw-tui -p cayw-webui --bin cayw-webui
cargo build --release --locked --target "$target" --target-dir "$target_dir" --manifest-path examples/headless-client/Cargo.toml --bin cayw-client
for program in cayw cayw-client cayw-tui cayw-webui; do
    cp "$target_dir/$target/release/$program" "$stage/bin/$program"
done
cp scripts/install-cli.sh "$stage/install.sh"
cp LICENSE "$stage/LICENSE"
mkdir "$stage/third-party"
cp third-party/CDLA-Permissive-2.0.txt "$stage/third-party/CDLA-Permissive-2.0.txt"
cp third-party/pi-MIT.txt "$stage/third-party/pi-MIT.txt"
# Do not label binaries with a commit that changed while Cargo was running.
[ "$(git rev-parse HEAD)" = "$revision" ] && git diff --quiet HEAD -- || {
    echo 'tracked source changed during packaging; refusing to label this bundle' >&2
    exit 1
}
printf '{"source_commit":"%s","target":"aarch64-apple-darwin","runtime_features":["plugins"],"version":"0.1.0"}\n' "$revision" > "$stage/BUILD.json"
(cd "$stage" && shasum -a 256 bin/cayw bin/cayw-client bin/cayw-tui bin/cayw-webui install.sh BUILD.json LICENSE THIRD-PARTY-NOTICES third-party/CDLA-Permissive-2.0.txt third-party/pi-MIT.txt > SHA256SUMS)
for program in cayw cayw-client cayw-tui cayw-webui; do "$stage/bin/$program" --help >/dev/null; done
mv "$stage" "$output"
printf 'Bundle assembled: %s\n' "$output"
