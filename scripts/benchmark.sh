#!/usr/bin/env bash
#
# Contract benchmark runner and regression gate (Issue #406).
#
# Runs the `cargo bench` scenario suite, measures the built Wasm, merges the
# two, and gates the result against the committed baseline. Wall-clock cost is
# dominated by the archive scenarios, which rebuild ~110 proposals per
# repetition, so expect a few minutes on a cold cache.
#
# Usage:
#   scripts/benchmark.sh                 # run + gate (exit 1 on regression)
#   scripts/benchmark.sh --update        # run + adopt results as the new baseline
#   scripts/benchmark.sh --no-build      # reuse the existing Wasm, skip the release build
#   scripts/benchmark.sh --no-history    # do not write a dated history snapshot
#   scripts/benchmark.sh --quick         # 1 repetition instead of 5 (smoke only)
#
# Environment:
#   VAULTDAO_BENCH_REPS   repetitions per scenario (default 5)
#   VAULTDAO_THRESHOLD    degradation fraction that fails (default 0.05)

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
CONTRACT_DIR="$REPO_ROOT/contracts/vault"
RESULTS_DIR="$REPO_ROOT/benchmarks/results"
HISTORY_DIR="$REPO_ROOT/benchmarks/history"
BASELINE="$REPO_ROOT/benchmarks/baseline.json"
CURRENT="$RESULTS_DIR/current.json"
WASM_JSON="$RESULTS_DIR/wasm-size.json"
REPORT="$RESULTS_DIR/report.md"
PROM_OUT="$REPO_ROOT/monitoring/contract-bench.prom"
WASM_PATH="$CONTRACT_DIR/target/wasm32-unknown-unknown/release/vault_dao.wasm"

UPDATE=0
DO_BUILD=1
HISTORY=1
THRESHOLD="${VAULTDAO_THRESHOLD:-0.05}"

while [[ $# -gt 0 ]]; do
  case "$1" in
    --update)     UPDATE=1; shift ;;
    --no-build)   DO_BUILD=0; shift ;;
    --no-history) HISTORY=0; shift ;;
    --quick)      export VAULTDAO_BENCH_REPS=1; shift ;;
    -h|--help)  sed -n '2,/^[^#]/p' "${BASH_SOURCE[0]}" | sed '$d; s/^# \{0,1\}//'; exit 0 ;;
    *) echo "unknown option: $1" >&2; exit 2 ;;
  esac
done

export VAULTDAO_BENCH_OUT="$CURRENT"
# Recorded with the results so a Wasm size difference can be attributed to a
# toolchain change rather than a source change. See bench-regression.py.
export VAULTDAO_BENCH_TOOLCHAIN="$(rustc --version 2>/dev/null || echo unknown)"
: "${VAULTDAO_BENCH_REPS:=5}"
export VAULTDAO_BENCH_REPS

mkdir -p "$RESULTS_DIR"

echo "==> contract benchmarks (reps=$VAULTDAO_BENCH_REPS, threshold=$THRESHOLD)"

# The bench registers the contract as a native test contract, so it needs the
# testutils feature that lives in dev-dependencies; `cargo bench` already
# builds targets against those.
(cd "$CONTRACT_DIR" && cargo bench --bench contract_bench)

if [[ $DO_BUILD -eq 1 ]]; then
  echo "==> building release Wasm"
  (cd "$CONTRACT_DIR" && cargo build --target wasm32-unknown-unknown --release)
else
  echo "==> skipping Wasm build (--no-build)"
fi

if [[ ! -f "$WASM_PATH" ]]; then
  echo "error: $WASM_PATH not found" >&2
  echo "       run without --no-build, or build it manually:" >&2
  echo "       cargo build --manifest-path contracts/vault/Cargo.toml \\" >&2
  echo "         --target wasm32-unknown-unknown --release" >&2
  exit 2
fi

echo "==> measuring Wasm size"
python3 "$REPO_ROOT/scripts/wasm-size.py" "$WASM_PATH" --out "$WASM_JSON"

# Shared arguments for every gate invocation below. History is opt-out so a PR
# run does not leave an uncommitted dated snapshot behind.
GATE_ARGS=(
  --current "$CURRENT"
  --baseline "$BASELINE"
  --wasm-metrics "$WASM_JSON"
  --no-colour
)
if [[ $HISTORY -eq 1 ]]; then
  GATE_ARGS+=(--history "$HISTORY_DIR")
fi

if [[ $UPDATE -eq 1 ]]; then
  echo "==> updating baseline"
  python3 "$REPO_ROOT/scripts/bench-regression.py" \
    "${GATE_ARGS[@]}" \
    --prom-out "$PROM_OUT" \
    --report "$REPORT" \
    --update
  # A fresh baseline has nothing to compare against, so run the gate once more
  # to prove the new baseline is self-consistent.
  python3 "$REPO_ROOT/scripts/bench-regression.py" "${GATE_ARGS[@]}"
  exit 0
fi

if [[ ! -f "$BASELINE" ]]; then
  echo "==> no baseline yet; creating one"
  python3 "$REPO_ROOT/scripts/bench-regression.py" \
    "${GATE_ARGS[@]}" \
    --prom-out "$PROM_OUT" \
    --report "$REPORT" \
    --update
  echo
  echo "baseline created at $BASELINE - commit it so CI has a reference."
  exit 0
fi

echo "==> regression check"
set +e
python3 "$REPO_ROOT/scripts/bench-regression.py" \
  "${GATE_ARGS[@]}" \
  --threshold "$THRESHOLD" \
  --prom-out "$PROM_OUT" \
  --report "$REPORT"
STATUS=$?
set -e

echo
echo "artifacts:"
echo "  results   $CURRENT"
echo "  wasm      $WASM_JSON"
echo "  report    $REPORT"
echo "  prometheus $PROM_OUT"

exit $STATUS
