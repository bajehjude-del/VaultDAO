//! VaultDAO contract benchmark suite (Issue #406).
//!
//! Scenarios are grouped to match the issue's taxonomy - single event, batch
//! events, queries, governance, archive - plus storage and memory readouts that
//! fall out of the same measurement.
//!
//! Every scenario follows the same shape:
//!
//! ```text
//! with_vault(setup, |v| { /* build state */ measure_call(v.env, || v.client.target(...)) })
//! ```
//!
//! Setup is paid outside the measured region and the host's budget resets on
//! each top-level invocation, so what lands in the baseline is the cost of the
//! target call alone.
//!
//! Run with `cargo bench --bench contract_bench`. Results are printed as a
//! table and written as JSON for the regression gate in
//! `scripts/bench-regression.py`.

mod common;

use common::*;
use soroban_sdk::testutils::Address as _;
use soroban_sdk::{Address, Symbol, Vec as SVec};
use vault_dao::fixture::{ConditionLogic, Priority, ProposalStatus, Role, TransferDetails};

// `Vec` is std's here: it holds harness results, not contract values.
use std::vec::Vec;

fn main() {
    let n = reps();
    let mut out: Vec<Measurement> = Vec::new();

    // ========================================================================
    // Single event - one event per transaction
    // ========================================================================
    out.push(run("set_role", "single_event", n, || {
        with_vault(Setup::default(), |v| {
            let target = Address::generate(v.env);
            let (_, res, ns) = measure_call(v.env, || {
                v.client.set_role(&v.admin, &target, &Role::Treasurer)
            });
            (res, ns)
        })
    }));

    out.push(run("update_threshold", "single_event", n, || {
        with_vault(Setup::default(), |v| {
            let (_, res, ns) = measure_call(v.env, || v.client.update_threshold(&v.admin, &1));
            (res, ns)
        })
    }));

    // ========================================================================
    // Batch events - N events in one transaction
    // ========================================================================
    // `batch_propose_transfers` emits proposal_created per transfer plus a
    // fee-estimate event, so event bytes scale with N up to MAX_BATCH_SIZE=10.
    for size in [1usize, 5, 10] {
        out.push(run(
            &format!("batch_propose_transfers_{size}"),
            "batch_event",
            n,
            || {
                with_vault(Setup::default(), |v| {
                    let mut transfers = SVec::new(v.env);
                    for _ in 0..size {
                        transfers.push_back(TransferDetails {
                            recipient: Address::generate(v.env),
                            token: v.token.clone(),
                            amount: AMOUNT / 10,
                        });
                    }
                    let (_, res, ns) = measure_call(v.env, || {
                        v.client.batch_propose_transfers(
                            &v.proposer,
                            &transfers,
                            &Priority::Normal,
                            &SVec::new(v.env),
                            &ConditionLogic::And,
                            &INSURANCE,
                        )
                    });
                    (res, ns)
                })
            },
        ));
    }

    // The inverse shape: many state writes, exactly one event.
    for size in [10usize, 50] {
        out.push(run(
            &format!("bulk_add_to_whitelist_{size}"),
            "batch_event",
            n,
            || {
                with_vault(Setup::default(), |v| {
                    let mut addrs = SVec::new(v.env);
                    for _ in 0..size {
                        addrs.push_back(Address::generate(v.env));
                    }
                    let (_, res, ns) =
                        measure_call(v.env, || v.client.bulk_add_to_whitelist(&v.admin, &addrs));
                    (res, ns)
                })
            },
        ));
    }

    // N executions in one call: the heaviest per-event fan-out in the contract.
    out.push(run("batch_execute_proposals_5", "batch_event", n, || {
        with_vault(
            Setup {
                extra_signers: 1,
                ..Default::default()
            },
            |v| {
                let mut ids = SVec::new(v.env);
                for _ in 0..5 {
                    let id = v.propose(AMOUNT);
                    v.client.approve_proposal(&v.approver, &id);
                    ids.push_back(id);
                }
                let (_, res, ns) = measure_call(v.env, || {
                    v.client.batch_execute_proposals(&v.proposer, &ids)
                });
                (res, ns)
            },
        )
    }));

    // ========================================================================
    // Queries - read-only paths
    // ========================================================================
    out.push(run("get_proposal", "query", n, || {
        with_vault(
            Setup {
                warm_proposals: 10,
                ..Default::default()
            },
            |v| {
                let (_, res, ns) = measure_call(v.env, || v.client.get_proposal(&1));
                (res, ns)
            },
        )
    }));

    out.push(run("get_config", "query", n, || {
        with_vault(Setup::default(), |v| {
            let (_, res, ns) = measure_call(v.env, || v.client.get_config());
            (res, ns)
        })
    }));

    // Index-backed page, capped at 50.
    out.push(run("get_proposals_by_status_50", "query", n, || {
        with_vault(
            Setup {
                warm_proposals: 60,
                ..Default::default()
            },
            |v| {
                let (_, res, ns) = measure_call(v.env, || {
                    v.client
                        .get_proposals_by_status(&ProposalStatus::Pending, &0, &50)
                });
                (res, ns)
            },
        )
    }));

    out.push(run("list_proposals_50", "query", n, || {
        with_vault(
            Setup {
                warm_proposals: 60,
                ..Default::default()
            },
            |v| {
                let (_, res, ns) = measure_call(v.env, || v.client.list_proposals(&0, &50));
                (res, ns)
            },
        )
    }));

    // The pathological one: a linear scan of every proposal id, reading and
    // discarding each. Cost grows with total proposal count, not page size,
    // which is exactly the kind of regression the gate should catch.
    out.push(run(
        "get_proposals_by_ledger_range_scan60",
        "query",
        n,
        || {
            with_vault(
                Setup {
                    warm_proposals: 60,
                    ..Default::default()
                },
                |v| {
                    let (_, res, ns) = measure_call(v.env, || {
                        v.client
                            .get_proposals_by_ledger_range(&0, &u64::MAX, &0, &50)
                    });
                    (res, ns)
                },
            )
        },
    ));

    out.push(run("get_audit_trail_50", "query", n, || {
        with_vault(
            Setup {
                audit_entries: 60,
                ..Default::default()
            },
            |v| {
                let (_, res, ns) = measure_call(v.env, || v.client.get_audit_trail(&0, &50));
                (res, ns)
            },
        )
    }));

    out.push(run("get_metrics", "query", n, || {
        with_vault(
            Setup {
                warm_proposals: 20,
                ..Default::default()
            },
            |v| {
                let (_, res, ns) = measure_call(v.env, || v.client.get_metrics());
                (res, ns)
            },
        )
    }));

    // ========================================================================
    // Governance - the full lifecycle
    // ========================================================================
    out.push(run("propose_transfer", "governance", n, || {
        with_vault(Setup::default(), |v| {
            let (_, res, ns) = measure_call(v.env, || v.propose(AMOUNT));
            (res, ns)
        })
    }));

    out.push(run("approve_proposal", "governance", n, || {
        with_vault(Setup::default(), |v| {
            let id = v.propose(AMOUNT);
            let (_, res, ns) = measure_call(v.env, || v.client.approve_proposal(&v.proposer, &id));
            (res, ns)
        })
    }));

    out.push(run("execute_proposal", "governance", n, || {
        with_vault(Setup::default(), |v| {
            let id = v.propose(AMOUNT);
            v.client.approve_proposal(&v.proposer, &id);
            let (_, res, ns) = measure_call(v.env, || v.client.execute_proposal(&v.proposer, &id));
            (res, ns)
        })
    }));

    out.push(run("cancel_proposal_rejection", "governance", n, || {
        with_vault(Setup::default(), |v| {
            let id = v.propose(AMOUNT);
            // Admin cancelling someone else's proposal takes the rejection arm,
            // which slashes both insurance and stake.
            let (_, res, ns) = measure_call(v.env, || {
                v.client
                    .cancel_proposal(&v.admin, &id, &Symbol::new(v.env, "bench"))
            });
            (res, ns)
        })
    }));

    // ========================================================================
    // Archive - audit trail checkpointing
    // ========================================================================
    // 100-entry Merkle root plus 100 Persistent deletes in one call.
    out.push(run("create_audit_checkpoint_100", "archive", n, || {
        with_vault(
            Setup {
                audit_entries: 110,
                ..Default::default()
            },
            |v| {
                let (_, res, ns) =
                    measure_call(v.env, || v.client.create_audit_checkpoint(&v.admin));
                (res, ns)
            },
        )
    }));

    out.push(run("verify_audit_chain_100", "archive", n, || {
        with_vault(
            Setup {
                audit_entries: 110,
                ..Default::default()
            },
            |v| {
                let (_, res, ns) = measure_call(v.env, || v.client.verify_audit_chain(&1, &100));
                (res, ns)
            },
        )
    }));

    out.push(run("get_audit_checkpoint", "archive", n, || {
        with_vault(
            Setup {
                audit_entries: 110,
                ..Default::default()
            },
            |v| {
                v.client.create_audit_checkpoint(&v.admin);
                let (_, res, ns) = measure_call(v.env, || v.client.get_audit_checkpoint(&1));
                (res, ns)
            },
        )
    }));

    report(&out);
}

/// Human-readable table on stdout, machine-readable JSON to disk.
///
/// The JSON is hand-rolled to keep `serde` out of the dependency tree - the
/// repo gates on `cargo audit` in CI and a benchmark harness is not worth
/// adding a dependency to.
fn report(ms: &[Measurement]) {
    println!(
        "\n{:<36} {:>10} {:>9} {:>7} {:>9} {:>9} {:>9} {:>12}",
        "scenario",
        "gas(instr)",
        "mem_bytes",
        "rd_ent",
        "wr_ent",
        "rd_bytes",
        "wr_bytes",
        "wall(median)"
    );
    println!("{}", "-".repeat(116));
    for m in ms {
        println!(
            "{:<36} {:>10} {:>9} {:>7} {:>9} {:>9} {:>9} {:>10}us",
            m.name,
            m.resources.instructions,
            m.memory_bytes(),
            m.storage_reads(),
            m.storage_writes(),
            m.storage_read_bytes(),
            m.storage_write_bytes(),
            m.wall_ns_median / 1000,
        );
    }

    // `cargo bench` runs with the crate directory as the working directory, so a
    // bare relative default would scatter results into
    // contracts/vault/benchmarks/. Anchor to the manifest and walk up to the
    // repo root so a direct `cargo bench` lands in the same place the runner
    // script does. `scripts/benchmark.sh` overrides this explicitly anyway.
    let path = std::env::var("VAULTDAO_BENCH_OUT").unwrap_or_else(|_| {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("benchmarks/results/current.json")
            .to_string_lossy()
            .into_owned()
    });
    if let Some(parent) = std::path::Path::new(&path).parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let json = to_json(ms);
    if let Err(e) = std::fs::write(&path, &json) {
        eprintln!("\nwarning: could not write {path}: {e}");
    } else {
        println!("\nwrote {path} ({} scenarios)", ms.len());
    }
}

fn to_json(ms: &[Measurement]) -> String {
    let mut s = format!(
        "{{\n  \"schema\": 1,\n  \"toolchain\": \"{}\",\n  \"scenarios\": [\n",
        toolchain()
    );
    for (i, m) in ms.iter().enumerate() {
        let r = &m.resources;
        // Collect the members first so the separator logic lives in one place
        // and no field can end up with a dangling comma.
        let fields: Vec<String> = vec![
            q("name", &m.name),
            q("group", &m.group),
            n("gas_instructions", r.instructions),
            n("memory_bytes", r.mem_bytes),
            n("storage_read_entries", r.read_entries),
            n("storage_write_entries", r.write_entries),
            n("storage_read_bytes", r.read_bytes),
            n("storage_write_bytes", r.write_bytes),
            n("event_bytes", r.event_bytes),
            n(
                "persistent_rent_ledger_bytes",
                r.persistent_rent_ledger_bytes,
            ),
            n("fee_total", r.fee_total),
            n("rent_fee", r.rent_fee),
            n("wall_ns_median", m.wall_ns_median),
            n("wall_ns_min", m.wall_ns_min),
            n("reps", m.reps),
        ];
        s.push_str("    {");
        s.push_str(&fields.join(", "));
        s.push('}');
        if i + 1 != ms.len() {
            s.push(',');
        }
        s.push('\n');
    }
    s.push_str("  ]\n}\n");
    s
}

/// `"key": "value"`
fn q(key: &str, val: &str) -> String {
    format!("\"{key}\": \"{val}\"")
}

/// `"key": 123`
fn n(key: &str, val: impl std::fmt::Display) -> String {
    format!("\"{key}\": {val}")
}
