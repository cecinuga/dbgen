#!/usr/bin/env bash
# Runs a series of loads, one configuration after another, so their throughput figures
# do not disturb each other. For each configuration: create its database and apply its
# DDL, empty the generated tables, load, verify, and keep a JSON report and a log.
#
# Usage:
#   scripts/run-stress-suite.sh [--no-verify] CONFIG...
#   scripts/run-stress-suite.sh examples/agevola/stress/*.yaml
#
# Environment:
#   BIN=bin/dbgen        the dbgen binary (scripts/cargo-docker.sh build --release)
#   REPORTS=reports      where reports and logs go
#
# Every generated table of every target database is emptied first (--reset). A failed
# configuration stops the series; the exit code is dbgen's.
set -uo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="${BIN:-$here/bin/dbgen}"
REPORTS="${REPORTS:-$here/reports}"
VERIFY=(--verify)
CONFIGS=()

for arg in "$@"; do
  case "$arg" in
    --no-verify) VERIFY=() ;;
    -h|--help) sed -n '2,17p' "$0"; exit 0 ;;
    -*) echo "run-stress-suite: unknown option '$arg'" >&2; exit 2 ;;
    *) CONFIGS+=("$arg") ;;
  esac
done
if [ "${#CONFIGS[@]}" -eq 0 ]; then
  echo "run-stress-suite: give one or more configuration files" >&2
  exit 2
fi
if [ ! -x "$BIN" ]; then
  echo "run-stress-suite: $BIN not found; build it with scripts/cargo-docker.sh build --release" >&2
  exit 2
fi
mkdir -p "$REPORTS"

for config in "${CONFIGS[@]}"; do
  name="$(basename "$config" .yaml)"
  log="$REPORTS/$name.log"
  echo "=== $name: applying the DDL (log: $log)"
  "$BIN" apply -c "$config" --create-database >"$log" 2>&1 || { tail -5 "$log" >&2; exit 1; }
  echo "=== $name: loading"
  "$BIN" generate -c "$config" --reset --no-progress "${VERIFY[@]}" --report "$REPORTS/$name.json" >>"$log" 2>&1
  code=$?
  sed -n '/^Total Target Rows/,/^Overall Status/p' "$log"
  grep -E '^Passed: ' "$log"
  if [ "$code" -ne 0 ]; then
    echo "run-stress-suite: $name failed with exit code $code; see $log" >&2
    exit "$code"
  fi
done
echo "=== series complete; reports in $REPORTS"
