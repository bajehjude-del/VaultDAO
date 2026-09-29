#!/usr/bin/env python3
"""Contract benchmark regression gate (Issue #406).

Compares the current benchmark run against a committed baseline and fails when
any gated metric degrades by more than a threshold (5% by default).

What is and is not gated is a deliberate decision, documented here because it
is the difference between a useful signal and noise nobody trusts:

GATED (higher is worse, these are deterministic per-commit values)
    gas_instructions           - the "gas" a user is charged for
    memory_bytes               - modelled peak memory
    storage_read_entries       - ledger entries touched on read
    storage_write_entries      - ledger entries touched on write
    storage_read_bytes         - ledger bytes read
    storage_write_bytes        - ledger bytes written
    event_bytes                - on-chain event payload size
    wasm_raw_bytes             - deployment footprint
    wasm_gzip_bytes            - upload footprint
    contractspecv0_size_bytes  - contract interface size

REPORTED BUT NOT GATED
    wall_ns_median/min   Wall time is dominated by host noise on shared CI
                         runners. It is the right signal for a human staring
                         at a local run and the wrong signal for a merge gate.
    fee_total, rent_fee  In the native test harness rent bumping dominates the
    persistent_rent_...  fee, and its magnitude depends on the test env's
                         rent config rather than on chain economics. Reporting
                         it would create a gate that fires on unrelated changes.

Because every gated value is deterministic for a given commit, a >5% move is a
real behavioural change and not measurement error.

Usage:
    bench-regression.py --current results.json --baseline benchmarks/baseline.json
    bench-regression.py ... --update          # accept current as the new baseline
    bench-regression.py ... --history         # also write a dated snapshot
    bench-regression.py ... --threshold 0.10
"""

import argparse
import json
import os
import sys
from datetime import datetime, timezone

# Higher is worse, and deterministic across runs of the same commit.
GATED_SCENARIO_METRICS = (
    "gas_instructions",
    "memory_bytes",
    "storage_read_entries",
    "storage_write_entries",
    "storage_read_bytes",
    "storage_write_bytes",
    "event_bytes",
)

# Tracked and shown, but never fails the build.
UNGATED_SCENARIO_METRICS = (
    "wall_ns_median",
    "wall_ns_min",
    "fee_total",
    "rent_fee",
    "persistent_rent_ledger_bytes",
)

# Whole-build metrics, compared once rather than per scenario.
GATED_GLOBAL_METRICS = (
    "wasm_raw_bytes",
    "wasm_gzip_bytes",
    "contractspecv0_size_bytes",
)

# An increase matters even against a zero baseline, where a percentage is
# undefined. Requiring a visible absolute cost stops a new metric from being
# waved through, without inventing a ratio.
# A baseline of exactly 0 means "this cost did not exist before". Any newly
# appearing cost is therefore a regression regardless of ratio. Compared with
# `>=` so that a single new storage write - 1 entry - still trips the gate;
# a strict `>` against a limit of 1 would let the very cheapest regressions
# through.
ZERO_BASELINE_ABSOLUTE_LIMIT = 0

RESET = "\033[0m"
RED = "\033[31m"
GREEN = "\033[32m"
YELLOW = "\033[33m"
BOLD = "\033[1m"


def colour(text, code, enabled):
    return f"{code}{text}{RESET}" if enabled else text


def load(path, required=True):
    if not path:
        if required:
            sys.exit("error: a required input path was empty")
        return None
    if not os.path.isfile(path):
        if required:
            sys.exit(f"error: {path} not found")
        return None
    with open(path) as fh:
        return json.load(fh)


def pct_change(base, current):
    """Percentage change, or None when the baseline is zero."""
    if base == 0:
        return None
    return ((current - base) / abs(base)) * 100.0


def fmt_int(n):
    return f"{n:,}"


def merge_globals(current, wasm_metrics):
    """Fold the wasm-size report into the results document."""
    if wasm_metrics:
        current.setdefault("global_metrics", {}).update(wasm_metrics)
    return current


def build_index(doc):
    return {s["name"]: s for s in doc.get("scenarios", [])}


def compare(threshold_pct, current, baseline, use_colour):
    """Return (regressions, warnings, improvements, notes).

    Modelled resources and Wasm size are held to different standards on
    purpose. Resources are a property of the contract and reproduce across
    toolchains. Wasm size is a property of the source *and* the compiler, so if
    the toolchain changed since the baseline was taken, a size move is reported
    as a warning rather than failed - otherwise a routine `stable` bump would
    block unrelated PRs with a size difference nobody caused.
    """
    threshold = threshold_pct * 100.0
    cur_idx = build_index(current)
    base_idx = build_index(baseline)

    cur_tc = current.get("toolchain")
    base_tc = baseline.get("toolchain")
    size_is_comparable = cur_tc == base_tc

    regressions, warnings, improvements, notes = [], [], [], []

    for name, cur in cur_idx.items():
        base = base_idx.get(name)
        if base is None:
            notes.append(
                colour(f"  NEW      {name:<36} no baseline entry; run with --update to adopt",
                       YELLOW, use_colour)
            )
            continue
        for metric in GATED_SCENARIO_METRICS:
            c, b = cur.get(metric), base.get(metric)
            if c is None or b is None:
                continue
            change = pct_change(b, c)
            if change is None:
                if c >= max(ZERO_BASELINE_ABSOLUTE_LIMIT, 1):
                    regressions.append(
                        (name, metric, b, c, None, f"new cost {c} (baseline was 0)")
                    )
                continue
            if change > threshold:
                regressions.append((name, metric, b, c, change, ""))
            elif change < -threshold:
                improvements.append((name, metric, b, c, change))

    for name in base_idx:
        if name not in cur_idx:
            notes.append(
                colour(f"  REMOVED  {name:<36} present in baseline, absent from this run",
                       YELLOW, use_colour)
            )

    if not size_is_comparable and (cur_tc or base_tc):
        notes.append(colour(
            f"  NOTE     toolchain changed: baseline {base_tc!r} -> current {cur_tc!r}",
            YELLOW, use_colour))
        notes.append(colour(
            "          Wasm size differences are reported but not failed, because they\n"
            "          are not comparable across compilers. Re-baseline to re-enable the gate.",
            YELLOW, use_colour))

    base_globals = baseline.get("global_metrics", {})
    cur_globals = current.get("global_metrics", {})
    for metric in GATED_GLOBAL_METRICS:
        c, b = cur_globals.get(metric), base_globals.get(metric)
        if c is None or b is None:
            continue
        change = pct_change(b, c)
        if change is None:
            if c >= max(ZERO_BASELINE_ABSOLUTE_LIMIT, 1):
                (regressions if size_is_comparable else warnings).append(
                    ("(wasm)", metric, b, c, None, f"new cost {c} (baseline was 0)")
                )
            continue
        if change > threshold:
            entry = ("(wasm)", metric, b, c, change, "")
            (regressions if size_is_comparable else warnings).append(entry)
        elif change < -threshold:
            improvements.append(("(wasm)", metric, b, c, change))

    return regressions, warnings, improvements, notes


def print_delta_table(current, use_colour):
    """Full per-scenario table, for the PR log."""
    print()
    print(colour("  contract benchmark results", BOLD, use_colour))
    print("  " + "-" * 108)
    print(f"  {'scenario':<36} {'group':<13} {'gas(instr)':>12} {'storage r/w':>12} "
          f"{'events(B)':>10} {'wall(us)':>9}")
    print("  " + "-" * 108)
    for s in sorted(current.get("scenarios", []), key=lambda x: (x.get("group", ""), x["name"])):
        rw = f"{s.get('storage_read_entries', 0)}/{s.get('storage_write_entries', 0)}"
        print(f"  {s['name']:<36} {s.get('group', ''):<13} "
              f"{fmt_int(s.get('gas_instructions', 0)):>12} {rw:>12} "
              f"{fmt_int(s.get('event_bytes', 0)):>10} "
              f"{s.get('wall_ns_median', 0) // 1000:>9}")
    g = current.get("global_metrics", {})
    if g:
        print("  " + "-" * 108)
        print(f"  {'wasm raw':<36} {g.get('wasm_raw_bytes', 0):>12} bytes   "
              f"gzip {g.get('wasm_gzip_bytes', 0):,} bytes")
        if g.get("contractspecv0_size_bytes"):
            print(f"  {'contractspecv0 (interface)':<36} "
                  f"{g.get('contractspecv0_size_bytes', 0):>12} bytes")
    print()


def write_prometheus(path, current):
    """Emit Prometheus text format so the existing stack can scrape it."""
    os.makedirs(os.path.dirname(path) or ".", exist_ok=True)
    lines = [
        "# HELP vaultdao_contract_gas_instructions Modelled CPU instructions for one contract invocation.",
        "# TYPE vaultdao_contract_gas_instructions gauge",
    ]
    for s in current.get("scenarios", []):
        labels = f'scenario="{s["name"]}",group="{s.get("group", "unknown")}"'
        lines.append(f"vaultdao_contract_gas_instructions{{{labels}}} {s.get('gas_instructions', 0)}")

    lines += [
        "# HELP vaultdao_contract_storage_read_entries Ledger entries read per invocation.",
        "# TYPE vaultdao_contract_storage_read_entries gauge",
    ]
    for s in current.get("scenarios", []):
        labels = f'scenario="{s["name"]}",group="{s.get("group", "unknown")}"'
        lines.append(f"vaultdao_contract_storage_read_entries{{{labels}}} {s.get('storage_read_entries', 0)}")

    lines += [
        "# HELP vaultdao_contract_storage_write_entries Ledger entries written per invocation.",
        "# TYPE vaultdao_contract_storage_write_entries gauge",
    ]
    for s in current.get("scenarios", []):
        labels = f'scenario="{s["name"]}",group="{s.get("group", "unknown")}"'
        lines.append(f"vaultdao_contract_storage_write_entries{{{labels}}} {s.get('storage_write_entries', 0)}")

    lines += [
        "# HELP vaultdao_contract_memory_bytes Modelled peak memory per invocation.",
        "# TYPE vaultdao_contract_memory_bytes gauge",
    ]
    for s in current.get("scenarios", []):
        labels = f'scenario="{s["name"]}",group="{s.get("group", "unknown")}"'
        lines.append(f"vaultdao_contract_memory_bytes{{{labels}}} {s.get('memory_bytes', 0)}")

    g = current.get("global_metrics", {})
    if g:
        lines += [
            "# HELP vaultdao_contract_wasm_bytes Deployed contract size.",
            "# TYPE vaultdao_contract_wasm_bytes gauge",
            f'vaultdao_contract_wasm_bytes{{kind="raw"}} {g.get("wasm_raw_bytes", 0)}',
            f'vaultdao_contract_wasm_bytes{{kind="gzip"}} {g.get("wasm_gzip_bytes", 0)}',
            f'vaultdao_contract_wasm_bytes{{kind="contractspecv0"}} {g.get("contractspecv0_size_bytes", 0)}',
        ]

    with open(path, "w") as fh:
        fh.write("\n".join(lines) + "\n")


def write_markdown(path, current, regressions, threshold_pct):
    """Human-facing report, used for the PR comment and weekly history."""
    stamp = datetime.now(timezone.utc).strftime("%Y-%m-%d %H:%M UTC")
    out = [f"## Contract benchmark report ({stamp})", ""]
    out.append(f"Regression threshold: **{threshold_pct * 100:.0f}%** on gated metrics.")
    out.append("")

    out.append("| Scenario | Group | Gas (instr) | Storage r/w | Event bytes | vs baseline |")
    out.append("| --- | --- | ---: | ---: | ---: | --- |")
    base = {s["name"]: s for s in current.get("_baseline_scenarios", [])}
    for s in sorted(current.get("scenarios", []), key=lambda x: (x.get("group", ""), x["name"])):
        b = base.get(s["name"])
        delta = ""
        if b and b.get("gas_instructions"):
            d = pct_change(b["gas_instructions"], s.get("gas_instructions", 0))
            if d is not None:
                arrow = "🔴" if d > threshold_pct * 100 else ("🟢" if d < -threshold_pct * 100 else "⚪")
                delta = f"{arrow} {d:+.1f}%"
        rw = f"{s.get('storage_read_entries', 0)}/{s.get('storage_write_entries', 0)}"
        out.append(
            f"| `{s['name']}` | {s.get('group', '')} | {fmt_int(s.get('gas_instructions', 0))} "
            f"| {rw} | {fmt_int(s.get('event_bytes', 0))} | {delta} |"
        )

    g = current.get("global_metrics", {})
    if g:
        out += ["", "### Build size", "",
                f"- Wasm raw: **{fmt_int(g.get('wasm_raw_bytes', 0))} bytes**",
                f"- Wasm gzip: **{fmt_int(g.get('wasm_gzip_bytes', 0))} bytes**",
                f"- `contractspecv0` (interface): **{fmt_int(g.get('contractspecv0_size_bytes', 0))} bytes**"]

    if regressions:
        out += ["", "### Regressions over threshold", "",
                "| Metric | Baseline | Current | Change |", "| --- | ---: | ---: | ---: |"]
        for name, metric, b, c, change, extra in regressions:
            chg = f"{change:+.1f}%" if change is not None else extra
            out.append(f"| `{name}` / `{metric}` | {fmt_int(b)} | {fmt_int(c)} | {chg} |")

    with open(path, "w") as fh:
        fh.write("\n".join(out) + "\n")


def main() -> int:
    ap = argparse.ArgumentParser(description="Contract benchmark regression gate")
    ap.add_argument("--current", required=True, help="current results JSON")
    ap.add_argument("--baseline", required=True, help="committed baseline JSON")
    ap.add_argument("--wasm-metrics", help="wasm-size.py output to merge in")
    ap.add_argument("--threshold", type=float, default=0.05,
                    help="degradation fraction that fails (default 0.05)")
    ap.add_argument("--update", action="store_true",
                    help="write current results as the new baseline and exit 0")
    ap.add_argument("--history", help="also write a dated snapshot to this directory")
    ap.add_argument("--prom-out", help="write Prometheus text format here")
    ap.add_argument("--report", help="write a markdown report here")
    ap.add_argument("--no-colour", action="store_true")
    args = ap.parse_args()

    use_colour = not args.no_colour and sys.stdout.isatty()

    current = merge_globals(load(args.current), load(args.wasm_metrics, required=False))
    baseline = load(args.baseline, required=not args.update)

    # Write the merged document back so the on-disk artifact is self-contained:
    # current.json, the baseline, the history snapshot and the Prometheus file
    # then all describe the same run.
    with open(args.current, "w") as fh:
        json.dump(current, fh, indent=2, sort_keys=True)
        fh.write("\n")

    if args.prom_out:
        write_prometheus(args.prom_out, current)

    if args.update:
        os.makedirs(os.path.dirname(args.baseline) or ".", exist_ok=True)
        with open(args.baseline, "w") as fh:
            json.dump(current, fh, indent=2, sort_keys=True)
            fh.write("\n")
        print(f"baseline updated: {args.baseline} "
              f"({len(current.get('scenarios', []))} scenarios)")

    if baseline is None:
        print("no baseline to compare against; treating this run as the reference")
        if args.history:
            write_history(args.history, current)
        return 0

    # Stash baseline scenarios so the markdown report can show deltas.
    report_doc = dict(current)
    report_doc["_baseline_scenarios"] = baseline.get("scenarios", [])

    regressions, warnings, improvements, notes = compare(
        args.threshold, current, baseline, use_colour)
    print_delta_table(current, use_colour)

    for n in notes:
        print(n)
    for name, metric, b, c, change in improvements:
        chg = f"{change:+.1f}%"
        print(colour(f"  IMPROVED {name:<36} {metric:<26} {fmt_int(b)} -> {fmt_int(c)}  {chg}",
                     GREEN, use_colour))
    print()

    for name, metric, b, c, change, extra in warnings:
        chg = f"{change:+.1f}%" if change is not None else extra
        print(colour(f"  WARN     {name:<36} {metric:<26} {fmt_int(b)} -> {fmt_int(c)}  {chg}",
                     YELLOW, use_colour))
    if warnings:
        print()

    if regressions:
        print(colour(
            f"  REGRESSION: {len(regressions)} metric(s) degraded more than "
            f"{args.threshold * 100:.0f}%", RED + BOLD, use_colour))
        for name, metric, b, c, change, extra in regressions:
            chg = f"{change:+.1f}%" if change is not None else extra
            print(colour(f"    {name:<36} {metric:<26} {fmt_int(b)} -> {fmt_int(c)}  {chg}",
                         RED, use_colour))
        print()
        print("  If this change is an intentional cost increase, re-baseline with:")
        print("    python3 scripts/bench-regression.py --current benchmarks/results/current.json \\")
        print("      --baseline benchmarks/baseline.json --update")
    elif warnings:
        print(colour(
            f"  OK: no gated metric degraded more than {args.threshold * 100:.0f}%"
            " (Wasm size not gated across a toolchain change)",
            GREEN + BOLD, use_colour))
    else:
        print(colour(f"  OK: no gated metric degraded more than {args.threshold * 100:.0f}%",
                     GREEN + BOLD, use_colour))

    if args.report:
        os.makedirs(os.path.dirname(args.report) or ".", exist_ok=True)
        write_markdown(args.report, report_doc, regressions, args.threshold)
        print(f"  report: {args.report}")

    if args.history:
        write_history(args.history, current)

    return 1 if regressions and not args.update else 0


def write_history(directory, current):
    """Dated snapshot for trend tracking.

    Filenames are date-stamped and the set is pruned to a bounded window so the
    directory stays useful as a time series instead of growing forever.
    """
    os.makedirs(directory, exist_ok=True)
    today = datetime.now(timezone.utc).strftime("%Y-%m-%d")
    path = os.path.join(directory, f"{today}.json")
    with open(path, "w") as fh:
        json.dump(current, fh, indent=2, sort_keys=True)
        fh.write("\n")
    print(f"  history: {path}")

    keep = 90  # ~3 months of daily snapshots
    snaps = sorted(f for f in os.listdir(directory) if f.endswith(".json"))
    for stale in snaps[:-keep]:
        os.remove(os.path.join(directory, stale))
        print(f"  pruned old snapshot: {stale}")


if __name__ == "__main__":
    sys.exit(main())
