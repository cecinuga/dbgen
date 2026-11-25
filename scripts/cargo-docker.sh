#!/usr/bin/env bash
# Runs cargo inside the official Rust image, for hosts without a Rust toolchain.
#
#   scripts/cargo-docker.sh build --release
#   scripts/cargo-docker.sh test
#   scripts/cargo-docker.sh run --release -- generate --config dbgen.yaml
#
# The registry cache and the target directory live in named Docker volumes. The
# container uses the host network, so a database published on localhost is reachable
# as localhost. Release binaries are copied to ./bin.
set -euo pipefail
here="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
image="${RUST_IMAGE:-rust:1-slim-bookworm}"
tty_args=()
if [ -t 1 ]; then tty_args=(-t); fi
docker run --rm "${tty_args[@]}" --network host \
  -e CARGO_HOME=/cargo-home -e CARGO_TARGET_DIR=/target \
  -e DBGEN_TEST_CLICKHOUSE_URL -e DBGEN_TEST_POSTGRES_URL -e DBGEN_TEST_MYSQL_URL -e RUST_BACKTRACE -e RUST_LOG \
  -v dbgen-cargo-home:/cargo-home -v dbgen-target:/target -v "$here:/work" -w /work \
  "$image" bash -c 'cargo "$@" && if [ -x /target/release/dbgen ]; then mkdir -p bin && cp -u /target/release/dbgen bin/; fi' cargo "$@"
