# Contract Benchmarking

Benchmarks for the VaultDAO Soroban contract, with a CI gate that fails a pull
request when the contract gets measurably more expensive.

- Harness: `contracts/vault/benches/contract_bench.rs` + `contracts/vault/benches/common/mod.rs`
- Runner: `scripts/benchmark.sh`
- Gate: `scripts/bench-regression.py`
- Wasm sizing: `scripts/wasm-size.py`
- CI: `.github/workflows/benchmark.yml`
- Baseline: `benchmarks/baseline.json`
- Dashboards: `monitoring/grafana-benchmark-dashboard.json`, metrics in `monitoring/contract-bench.prom`

## Quick start

```bash
# Full run: benchmarks + wasm size + regression gate
scripts/benchmark.sh

# Smoke run (1 repetition instead of 5)
scripts/benchmark.sh --quick

# Accept the current numbers as the new reference, and commit the change
scripts/benchmark.sh --update
```

`scripts/benchmark.sh` exits non-zero when a gated metric degrades by more than
5%, which is what CI keys off.

## What is actually measured

**Wall-clock time is the wrong primary metric for a Soroban contract.** What a
user pays for is resource consumption, and the host meters that per top-level
invocation. So every scenario records two independent things.

### Resources (deterministic, gated)

The host resets its budget before every top-level contract invocation, so a
single call is metered in isolation with no contamination from the fixture that
built the state around it:

```rust
with_vault(setup, |v| {
    let id = v.propose(AMOUNT);                  // setup, not measured
    measure_call(v.env, || v.client.approve_proposal(&id))   // measured
});
```

`measure_call` starts an `Instant` immediately before the call and reads
`env.cost_estimate()` immediately after. From that we record:

| Metric | Source | Meaning |
| --- | --- | --- |
| `gas_instructions` | `resources().instructions` | modelled CPU; the "gas" users pay for |
| `memory_bytes` | `resources().mem_bytes` | modelled peak memory |
| `storage_read_entries` | `resources().read_entries` | ledger entries touched on read |
| `storage_write_entries` | `resources().write_entries` | ledger entries touched on write |
| `storage_read_bytes` | `resources().read_bytes` | ledger bytes read |
| `storage_write_bytes` | `resources().write_bytes` | ledger bytes written |
| `event_bytes` | `resources().contract_events_size_bytes` | on-chain event payload size |

### Wall time (noisy, reported only)

Median and minimum over N repetitions, each on a freshly built fixture.
`VAULTDAO_BENCH_REPS` controls N (default 5, 3 on PRs, 10 weekly).

### Wasm size (deterministic, gated)

From `scripts/wasm-size.py`, which parses the built module's custom sections:

| Metric | Meaning |
| --- | --- |
| `wasm_raw_bytes` | deployment footprint |
| `wasm_gzip_bytes` | upload footprint, what actually crosses the network |
| `contractspecv0_size_bytes` | size of the embedded contract *interface* |

Tracking the interface separately is deliberate. It lets a size regression be
attributed to either an interface change (spec grew) or an implementation change
(spec unchanged, raw bytes grew) instead of leaving you guessing.

## The determinism check

The harness asserts that every deterministic field is **identical across all
repetitions**:

```rust
assert_eq!(*r, first, "scenario `{name}` produced non-deterministic resources on rep {i}...");
```

This is a real guard, not ceremony. It catches the failure mode where a scenario
accidentally measures its own setup, a clock, or a randomly-sized value, and
publishes a number that drifts on every run. Because the check passes, a 5% move
between two commits is a behavioural change rather than measurement noise — which
is the entire justification for gating on a 5% threshold.

It also forced one design decision: fixture funding is derived from the scenario's
own request (`contracts/vault/benches/common/mod.rs`) rather than hardcoded,
because stake escrow is auto-computed per proposal and a fixed amount silently
goes broke as soon as a scenario seeds more state.

## Scenarios

22 scenarios across the five groups named in the issue. Current cost ranking is
in `benchmarks/results/report.md`; the three heaviest on today's baseline:

| Scenario | Gas (instructions) | Why it is heavy |
| --- | ---: | --- |
| `create_audit_checkpoint_100` | 18,219,718 | 100-entry Merkle root plus 100 Persistent deletes |
| `list_proposals_50` | 9,690,773 | 102 ledger reads to materialise a 50-item page |
| `batch_execute_proposals_5` | 6,547,510 | 5 executions, each fanning out over approvers |

### `single_event` — one event per transaction
`set_role`, `update_threshold`. The floor cost of any state-changing call:
config read, a couple of writes, one audit entry.

### `batch_event` — N events in one transaction
`batch_propose_transfers_{1,5,10}` (event bytes scale with N, capped at
`MAX_BATCH_SIZE = 10`), `bulk_add_to_whitelist_{10,50}` (the inverse shape — many
writes, exactly one event), `batch_execute_proposals_5`.

### `query` — read-only paths
`get_proposal`, `get_config`, `get_proposals_by_status_50`, `list_proposals_50`,
`get_proposals_by_ledger_range_scan60`, `get_audit_trail_50`, `get_metrics`.

`get_proposals_by_ledger_range_scan60` is deliberately the pathological case: it
is a linear scan of every proposal id, so its cost tracks *total proposal count*
rather than page size. It is the canary for an accidental O(n) read path.

### `governance` — full lifecycle
`propose_transfer`, `approve_proposal`, `execute_proposal`,
`cancel_proposal_rejection`. The fixture drives real escrow and slashing, so
these include the insurance and stake costs a production call would pay.

### `archive` — audit trail
`create_audit_checkpoint_100`, `verify_audit_chain_100`, `get_audit_checkpoint`.
These rebuild ~110 proposals per repetition, which is why the suite is not
instant.

## What the gate does and does not check

**Gated** (higher is worse; all deterministic): `gas_instructions`,
`memory_bytes`, all four storage metrics, `event_bytes`, `wasm_raw_bytes`,
`wasm_gzip_bytes`, `contractspecv0_size_bytes`.

**Reported but never gated**, each for a specific reason:

- `wall_ns_median` / `wall_ns_min` — a shared CI runner's load is a property of
  the machine, not the commit. Gating on it produces flaky failures that train
  people to ignore the gate.
- `fee_total`, `rent_fee`, `persistent_rent_ledger_bytes` — in the native test
  harness, rent bumping dominates the fee estimate and its magnitude depends on
  the test environment's rent configuration rather than chain economics. A gate
  on these would fire on changes that cost nothing in production.

Improvements never fail the build; they are reported so a win is visible.

Special cases the gate handles explicitly:

- **Scenario added or removed** → reported as `NEW` / `REMOVED`, not a failure.
  A renamed scenario correctly shows as a pair rather than a false regression.
- **Baseline of zero** → a percentage is undefined, so any new non-zero cost is
  flagged as `new cost`. This is what catches a read path silently acquiring a
  write (e.g. a stray TTL bump): `list_proposals_50` has
  `storage_write_entries == 0`, so any non-zero value is a real finding.
- **First run** → no baseline means the run becomes the baseline.
- **Toolchain change** → modelled resources are a property of the contract, but
  Wasm *size* is a property of the source **and** the compiler. The repo does not
  pin a toolchain and CI tracks floating `stable`, so a `rustc` bump produces a
  different module for byte-identical source. Every run therefore records its
  `rustc --version` next to the results, and if it differs from the baseline's,
  size regressions are reported as `WARN` instead of failing the job. Resource
  regressions still fail, because they are genuinely comparable. Re-baselining
  re-enables the size gate.

## Re-baselining

A legitimate cost increase should be adopted, not fought:

```bash
scripts/benchmark.sh --update
git add benchmarks/baseline.json benchmarks/history
git commit -m "chore(benchmarks): re-baseline after <reason>"
```

Review the diff before committing — the point of the gate is that the increase
was a decision, not a surprise. The diff will also show a `toolchain` line change
if the compiler moved, which is worth calling out in the commit message.

`scripts/benchmark.sh --no-history` runs the gate without writing a dated
snapshot. CI passes it on pull requests so a PR never leaves uncommitted churn
behind; the weekly and manual runs record history.

## Historical tracking

- `benchmarks/baseline.json` — the committed gate reference (tracked).
- `benchmarks/history/YYYY-MM-DD.json` — dated full snapshots (tracked, written
  unless `--no-history`, pruned to the most recent 90).
- `benchmarks/results/` — per-run artifacts, gitignored.
- CI uploads results, reports, and snapshots as artifacts with 90-day retention.
  The weekly run additionally commits its dated snapshot back to `main`.

## Visualization

`monitoring/contract-bench.prom` is regenerated on every run in Prometheus text
format, using the existing `vaultdao_*` naming:

```
vaultdao_contract_gas_instructions{scenario="execute_proposal",group="governance"}
vaultdao_contract_storage_read_entries{...}
vaultdao_contract_storage_write_entries{...}
vaultdao_contract_memory_bytes{...}
vaultdao_contract_wasm_bytes{kind="raw|gzip|contractspecv0"}
```

`monitoring/grafana-benchmark-dashboard.json` (uid `vaultdao-contract-benchmarks`)
plots gas, storage reads/writes, memory, size, a fee-equivalent view, and a
top-N ranking, with `group` and `scenario` template variables.
`monitoring/prometheus-rules.yaml` adds secondary alerts on the same series.

## Limitations

Read these before quoting a number as an on-chain cost.

1. **Native, not Wasm.** The contract is registered as a native test contract,
   so the host does not model VM instantiation, Wasm reads, or Wasm rent bumps.
   The SDK's own docs state the numbers "mostly correspond only to the operations
   that have happened during the host invocation" and that instruction counts
   tend to be *underestimated* relative to the Wasm equivalent.

2. **Relative signal only.** Because of (1), these numbers are good for
   comparing two commits and detecting regressions. They are not a fee quote. For
   absolute on-chain cost, measure against a real network or a Wasm-executing
   host.

3. **Fee config is a snapshot.** The fee estimate uses Stellar Pubnet rates
   frozen at 2024-12-11. Fine for relative comparison; not for billing.

4. **Scenarios are single-tenant.** Each runs against a freshly built vault with
   a handful of signers. Multi-tenant or high-contention behaviour is out of
   scope here — that is what `load-tests/` (k6, against the backend API) is for.
   The two suites measure different layers on purpose and neither substitutes
   for the other.

5. **The suite is not exhaustive.** 22 scenarios across 360+ contract functions.
   Coverage is deliberately aimed at the paths that batch, scan, or fan out, since
   those are where cost surprises live. A cheap `get_*` on an index is
   represented by its heavier siblings rather than measured individually.
