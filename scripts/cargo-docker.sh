#!/usr/bin/env bash
# Runs cargo inside the official Rust image, for hosts without a Rust toolchain.
#
#   scripts/cargo-docker.sh build --release
#   scripts/cargo-docker.sh test
#   scripts/cargo-docker.sh run --release -- generate --config dbgen.yaml
#
# The registry cache, installed cargo tools (cargo-audit...) and the target directory
# live in named Docker volumes. The container uses the host network, so a database
# published on localhost is reachable as localhost. Release binaries are copied to ./bin.
# clippy and rustfmt (and git, for cargo-deny), which the slim image lacks, are
# installed when needed.
set -euo pipefail
here="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
image="${RUST_IMAGE:-rust:1.100.0-slim-bookworm}"
tty_args=()
if [ -t 1 ]; then tty_args=(-t); fi
docker run --rm "${tty_args[@]}" --network host \
  -e CARGO_HOME=/cargo-home -e CARGO_TARGET_DIR=/target \
  -e PATH=/cargo-home/bin:/usr/local/cargo/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin \
  -e DBGEN_TEST_CLICKHOUSE_URL -e DBGEN_TEST_POSTGRES_URL -e DBGEN_TEST_MYSQL_URL -e RUST_BACKTRACE -e RUST_LOG \
  -e RUSTFLAGS -e RUSTDOCFLAGS \
  -v dbgen-cargo-home:/cargo-home -v dbgen-target:/target -v "$here:/work" -w /work \
  "$image" bash -c '
    case "$1" in
      clippy) rustup component add clippy >/dev/null 2>&1 ;;
      fmt) rustup component add rustfmt >/dev/null 2>&1 ;;
      # cargo-deny fetches its advisory database with the git command.
      deny) command -v git >/dev/null || { apt-get update -qq && apt-get install -y -qq --no-install-recommends git >/dev/null; } ;;
    esac
    cargo "$@" && if [ -x /target/release/dbgen ]; then mkdir -p bin && cp -u /target/release/dbgen bin/; fi' cargo "$@"
