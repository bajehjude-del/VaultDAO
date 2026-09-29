//! Shared harness for the VaultDAO contract benchmarks.
//!
//! # What is actually measured
//!
//! Wall-clock time is the wrong primary metric for a Soroban contract. What a
//! user pays for, and what the network meters, is *resource consumption*. The
//! host resets its budget before every top-level contract invocation, so a
//! single `client.foo()` call can be metered in isolation with no noise from
//! the fixture that built the state around it.
//!
//! For every scenario we therefore record two independent things:
//!
//! * **Resources** (`cost_estimate().resources()`) - instructions, ledger
//!   read/write bytes and entry counts, event bytes, and rent. These are
//!   *deterministic*: the same call on an equivalent state always yields
//!   identical numbers. They are the numbers that determine transaction fees.
//! * **Wall time** - median over N repetitions on freshly built fixtures, each
//!   timed with `Instant`. Useful for spotting algorithmic blowups, but noisy
//!   on shared CI runners, so it is reported and never gated on.
//!
//! The harness asserts that the deterministic fields really are identical
//! across repetitions. If they are not, the scenario is measuring its own
//! setup rather than the function under test, and the benchmark fails loudly
//! instead of publishing a meaningless number.
//!
//! # Interpretation caveats
//!
//! Resource numbers come from a native `Env` with the contract registered as a
//! test contract, not from the compiled Wasm. The host docs are explicit that
//! VM instantiation, Wasm reads, and rent bumps are therefore *not* fully
//! modelled, and that native execution tends to undercount instructions versus
//! the Wasm equivalent. Treat these as a **relative** signal for detecting
//! regressions between commits - which is exactly what the regression gate in
//! `scripts/bench-regression.py` consumes - and not as an absolute fee quote.
//! See `docs/guides/BENCHMARKING.md` for the full methodology.

use soroban_sdk::testutils::{Address as _, EnvTestConfig, Ledger as _};
use soroban_sdk::{token::StellarAssetClient, Address, Env};

use vault_dao::fixture::{
    ConditionLogic, InitConfig, InsuranceConfig, Priority, RecoveryConfig, RetryConfig, Role,
    StakingConfig, ThresholdStrategy, VelocityConfig, VoteWeight,
};
use vault_dao::{VaultDAO, VaultDAOClient};

/// Token amount used by scenarios that need a funded vault.
pub const AMOUNT: i128 = 1_000_000;
/// Insurance escrowed alongside a proposal in staking scenarios.
pub const INSURANCE: i128 = 10_000;

/// Deterministic resource footprint of one top-level contract invocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Resources {
    pub instructions: i64,
    pub mem_bytes: i64,
    pub read_entries: u32,
    pub write_entries: u32,
    pub read_bytes: u32,
    pub write_bytes: u32,
    pub event_bytes: u32,
    pub persistent_rent_ledger_bytes: i64,
    pub fee_total: i64,
    pub rent_fee: i64,
}

impl Resources {
    fn capture(env: &Env) -> Self {
        let r = env.cost_estimate().resources();
        let fee = env.cost_estimate().fee();
        Self {
            instructions: r.instructions,
            mem_bytes: r.mem_bytes,
            read_entries: r.read_entries,
            write_entries: r.write_entries,
            read_bytes: r.read_bytes,
            write_bytes: r.write_bytes,
            event_bytes: r.contract_events_size_bytes,
            persistent_rent_ledger_bytes: r.persistent_rent_ledger_bytes,
            fee_total: fee.total,
            rent_fee: fee.persistent_entry_rent + fee.temporary_entry_rent,
        }
    }
}

/// One scenario's published numbers.
#[derive(Debug, Clone)]
pub struct Measurement {
    pub name: String,
    pub group: String,
    pub resources: Resources,
    pub wall_ns_median: u64,
    pub wall_ns_min: u64,
    pub reps: usize,
}

impl Measurement {
    /// Storage reads and writes, split out because the issue calls for them
    /// as distinct metrics and they move independently of instruction count.
    pub fn storage_reads(&self) -> u32 {
        self.resources.read_entries
    }
    pub fn storage_writes(&self) -> u32 {
        self.resources.write_entries
    }
    pub fn storage_read_bytes(&self) -> u32 {
        self.resources.read_bytes
    }
    pub fn storage_write_bytes(&self) -> u32 {
        self.resources.write_bytes
    }
    /// Modelled peak memory for the invocation.
    pub fn memory_bytes(&self) -> i64 {
        self.resources.mem_bytes
    }
}

/// A live vault plus the handles a scenario needs.
pub struct Vault<'a> {
    pub env: &'a Env,
    pub client: VaultDAOClient<'a>,
    pub admin: Address,
    pub proposer: Address,
    pub approver: Address,
    pub token: Address,
}

/// Knobs for [`with_vault`], so a scenario can reshape the vault it is measured
/// against without rebuilding the whole fixture by hand.
#[derive(Clone)]
pub struct Setup {
    pub staking: StakingConfig,
    pub insurance: InsuranceConfig,
    /// Number of extra signers beyond admin + proposer. Approval cost grows
    /// with the approver set, so governance scenarios scale on this.
    pub extra_signers: u32,
    /// Pre-create this many proposals before the measured call.
    pub warm_proposals: u32,
    /// Push the ledger forward by this many ledgers before the measured call.
    pub advance_ledgers: u64,
    /// Populate the audit trail with this many entries (archive scenarios).
    pub audit_entries: u32,
}

impl Default for Setup {
    fn default() -> Self {
        Self {
            staking: staking_config(true, 10),
            insurance: InsuranceConfig {
                enabled: true,
                min_amount: 1,
                min_insurance_bps: 100,
                slash_percentage: 30,
            },
            extra_signers: 0,
            warm_proposals: 0,
            advance_ledgers: 0,
            audit_entries: 0,
        }
    }
}

pub fn staking_config(enabled: bool, slash_pct: u32) -> StakingConfig {
    StakingConfig {
        enabled,
        min_amount: 1,
        base_stake_bps: 1000, // 10% of the proposal amount
        max_stake_amount: i128::MAX,
        reputation_discount_threshold: 1000, // unreachable: no discount
        reputation_discount_percentage: 0,
        slash_percentage: slash_pct,
        cancellation_slash_percentage: 50,
        slash_to_insurance_pool: false,
        compound_lock_period: 17280,
        compound_epoch: 17280,
        reward_bps_per_execution: 0,
    }
}

/// Builds a fully initialised vault inside a fresh `Env` and hands it to `f`.
///
/// The `Env` is created here rather than by the caller so that
/// `VaultDAOClient<'a>` can borrow it without a self-referential struct. All
/// setup cost is paid *before* `f` runs, which is what lets scenarios time a
/// single call in isolation.
pub fn with_vault<F, R>(setup: Setup, f: F) -> R
where
    F: FnOnce(&Vault<'_>) -> R,
{
    let mut env = Env::default();
    // Without this every dropped Env writes a ledger snapshot JSON, which would
    // litter the tree with one file per benchmark repetition.
    env.set_config(EnvTestConfig {
        capture_snapshot_at_drop: false,
    });
    env.mock_all_auths();

    let id = env.register(VaultDAO, ());
    let client = VaultDAOClient::new(&env, &id);

    let admin = Address::generate(&env);
    let proposer = Address::generate(&env);
    let approver = Address::generate(&env);
    let token_admin = Address::generate(&env);
    let token = env
        .register_stellar_asset_contract_v2(token_admin.clone())
        .address();

    let mut signers = soroban_sdk::Vec::new(&env);
    signers.push_back(admin.clone());
    signers.push_back(proposer.clone());
    if setup.extra_signers > 0 {
        signers.push_back(approver.clone());
        for _ in 1..setup.extra_signers {
            signers.push_back(Address::generate(&env));
        }
    }

    client.initialize(
        &admin,
        &InitConfig {
            veto_window_ledgers: 0,
            whitelist_mode: false,
            grace_period_ledgers: 100,
            vote_weight: VoteWeight::Flat,
            high_impact_threshold: 70,
            admin_rotation_delay: 1440,
            signers,
            threshold: 1,
            quorum: 0,
            quorum_percentage: 0,
            default_voting_deadline: 0,
            spending_limit: i128::MAX / 4,
            daily_limit: i128::MAX / 4,
            weekly_limit: i128::MAX / 4,
            // Park the timelock and velocity limits out of reach so no scenario
            // silently starts paying a timelock the others do not.
            timelock_threshold: i128::MAX / 4,
            timelock_delay: 0,
            velocity_limit: VelocityConfig {
                limit: u32::MAX,
                window: 3600,
                per_token_limit: 0,
            },
            threshold_strategy: ThresholdStrategy::Fixed,
            pre_execution_hooks: soroban_sdk::Vec::new(&env),
            post_execution_hooks: soroban_sdk::Vec::new(&env),
            veto_addresses: soroban_sdk::Vec::new(&env),
            retry_config: RetryConfig {
                max_retry_delay: 0,
                enabled: false,
                max_retries: 0,
                initial_backoff_ledgers: 0,
            },
            recovery_config: RecoveryConfig::default(&env),
            staking_config: setup.staking.clone(),
            proposal_id_prefix: 0,
        },
    );

    // `initialize` persists staking under Config; the slashing paths read the
    // dedicated key, so set both explicitly.
    client.update_staking_config(&admin, &setup.staking);
    client.set_insurance_config(&admin, &setup.insurance);
    client.set_role(&admin, &proposer, &Role::Treasurer);
    if setup.extra_signers > 0 {
        client.set_role(&admin, &approver, &Role::Treasurer);
    }

    // Fund the vault and the proposer so proposals can escrow stake/insurance.
    //
    // A proposal escrows `INSURANCE` plus an *auto-computed* stake of
    // `amount * base_stake_bps / 10_000`, so the proposer drain grows with the
    // number of seeded proposals. Derive the funding from the scenario's own
    // request instead of hardcoding a figure that silently goes broke as soon
    // as a scenario seeds more state.
    let seeded = (setup.warm_proposals + setup.audit_entries) as i128;
    let auto_stake = if setup.staking.enabled {
        AMOUNT * setup.staking.base_stake_bps as i128 / 10_000
    } else {
        0
    };
    let per_proposal = INSURANCE + auto_stake;
    let proposer_funding = per_proposal * (seeded + 4) + AMOUNT;
    StellarAssetClient::new(&env, &token).mint(&proposer, &proposer_funding);
    // The vault pays out on execution, so it needs enough for every proposal a
    // scenario may execute, not just one.
    StellarAssetClient::new(&env, &token).mint(&id, &(AMOUNT * (seeded + 8)));

    let vault = Vault {
        env: &env,
        client,
        admin,
        proposer,
        approver,
        token,
    };

    for _ in 0..setup.warm_proposals {
        vault.propose(AMOUNT);
    }
    // `propose_transfer` is the public path that appends to the audit chain
    // (lib.rs:1324), so proposals are the only seeding available to a bench.
    // `initialize` already wrote entry 1, hence the -1.
    for _ in 0..setup.audit_entries.saturating_sub(1) {
        vault.propose(AMOUNT);
    }
    if setup.advance_ledgers > 0 {
        vault.advance(setup.advance_ledgers);
    }

    f(&vault)
}

impl Vault<'_> {
    /// Creates one ordinary transfer proposal, fully escrowed.
    pub fn propose(&self, amount: i128) -> u64 {
        self.client.propose_transfer(
            &self.proposer,
            &Address::generate(self.env),
            &self.token,
            &amount,
            &soroban_sdk::Symbol::new(self.env, "bench"),
            &Priority::Normal,
            &soroban_sdk::Vec::new(self.env),
            &ConditionLogic::And,
            &INSURANCE,
        )
    }

    pub fn advance(&self, ledgers: u64) {
        self.env
            .ledger()
            .with_mut(|li| li.sequence_number += ledgers as u32);
    }
}

/// Times one top-level call and captures its metered resources.
///
/// The `Instant` starts immediately before `f`, and resources are read
/// immediately after, so both reflect the call and nothing else.
pub fn measure_call<R>(env: &Env, f: impl FnOnce() -> R) -> (R, Resources, u64) {
    let start = std::time::Instant::now();
    let out = f();
    let ns = start.elapsed().as_nanos() as u64;
    let res = Resources::capture(env);
    (out, res, ns)
}

/// Runs one scenario `reps` times, each on a freshly built fixture.
///
/// Deterministic resource fields must agree across repetitions; a mismatch
/// means the scenario is measuring setup or randomness rather than the
/// function under test, so we panic instead of reporting a bogus baseline.
pub fn run<F>(name: &str, group: &str, reps: usize, f: F) -> Measurement
where
    F: Fn() -> (Resources, u64),
{
    let mut samples: Vec<(Resources, u64)> = Vec::with_capacity(reps);
    for _ in 0..reps {
        samples.push(f());
    }

    let first = samples[0].0;
    for (i, (r, _)) in samples.iter().enumerate().skip(1) {
        assert_eq!(
            *r, first,
            "scenario `{name}` produced non-deterministic resources on rep {i}: \
             {r:?} != {first:?}. The scenario is measuring something other than \
             the function under test; make the fixture fully deterministic."
        );
    }

    let mut walls: Vec<u64> = samples.iter().map(|(_, ns)| *ns).collect();
    walls.sort_unstable();
    let median = walls[walls.len() / 2];

    Measurement {
        name: name.to_string(),
        group: group.to_string(),
        resources: first,
        wall_ns_median: median,
        wall_ns_min: walls[0],
        reps,
    }
}

/// Repetitions per scenario. Override with `VAULTDAO_BENCH_REPS` to trade
/// wall-time stability against runtime. Resources are unaffected - one
/// invocation is already exact.
pub fn reps() -> usize {
    std::env::var("VAULTDAO_BENCH_REPS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(5)
}

/// Toolchain that produced these numbers, recorded alongside the results.
///
/// Modelled resources are a property of the contract, not the compiler, so they
/// are comparable across toolchains. Wasm *size* is not: a different rustc
/// produces a different module for identical source. The regression gate uses
/// this to downgrade a size mismatch to a warning instead of a false failure.
/// `scripts/benchmark.sh` sets the variable; a bare `cargo bench` records
/// "unknown".
pub fn toolchain() -> String {
    std::env::var("VAULTDAO_BENCH_TOOLCHAIN").unwrap_or_else(|_| "unknown".to_string())
}
