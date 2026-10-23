#!/bin/sh
# Offline validation on a stable source snapshot. Does not call real models.
set -eu

image=${CAYW_RUST_IMAGE:-rust@sha256:e14e87345b4d5964ddcc3491d27ee046a0f23820f340c3c1e24da6880141f7c0}
project_root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
validation_dir=${1:?Usage: scripts/validate-linux.sh NEW_OUTPUT_DIRECTORY}
mkdir "$validation_dir"
validation_dir=$(CDPATH= cd -- "$validation_dir" && pwd)
printf '%s\n' "$image" > "$validation_dir/image.txt"

# Validate a copied input set; never compile a source file while another editor
# writes it. The archive fingerprint identifies the exact copy that was tested.
COPYFILE_DISABLE=1 tar -C "$project_root" -cf "$validation_dir/source.tar" Cargo.toml Cargo.lock crates scripts

validation_exit=0
docker run --rm \
  --mount "type=bind,source=$validation_dir,target=/evidence" \
  --mount type=volume,source=cayw-runtime-rust195-target,target=/build \
  --mount type=volume,source=cayw-runtime-rust195-registry,target=/usr/local/cargo/registry \
  --env CARGO_TARGET_DIR=/build \
  "$image" sh -eu -c '
    validation_stage=prepare
    trap '\''validation_code=$?; printf "stage=%s exit_code=%s\n" "$validation_stage" "$validation_code" >> /evidence/result.log'\'' EXIT
    mkdir /work
    tar -xf /evidence/source.tar -C /work
    cd /work
    sha256sum /evidence/source.tar > /evidence/source.sha256
    uname -sm > /evidence/environment.log
    rustc --version --verbose >> /evidence/environment.log
    cargo --version >> /evidence/environment.log
    run_check() {
      validation_stage=$1
      shift
      validation_code=0
      "$@" > "/evidence/$validation_stage.log" 2>&1 || validation_code=$?
      printf "stage=%s exit_code=%s\n" "$validation_stage" "$validation_code" >> /evidence/result.log
      if [ "$validation_code" -ne 0 ]; then exit "$validation_code"; fi
    }
    run_check check cargo check --workspace --all-targets --all-features --locked
    run_check tests cargo test --workspace --all-features --locked
    run_check minimal-tests cargo test --workspace --no-default-features --locked
    run_check offline cargo run -p cayw-agent --example offline --locked
    run_check headless cargo run -p cayw-runtime --example offline --locked -- /evidence/headless-demo
    validation_stage=complete
  ' > "$validation_dir/container.log" 2>&1 || validation_exit=$?
printf 'docker_exit_code=%s\n' "$validation_exit" >> "$validation_dir/result.log"
exit "$validation_exit"
