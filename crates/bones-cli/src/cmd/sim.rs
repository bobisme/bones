//! `bn sim` — deterministic simulation campaign commands.
//!
//! `bn sim run` — execute a campaign across many seeds.
//! `bn sim replay` — replay a single seed with detailed trace output.

use std::path::Path;
use std::process;

use anyhow::Result;
use clap::{Args, Subcommand};
use serde::Serialize;

use crate::output::OutputMode;

/// Top-level arguments for `bn sim`.
#[derive(Args, Debug)]
pub struct SimArgs {
    #[command(subcommand)]
    pub command: SimCommand,
}

/// Simulation subcommands.
#[derive(Subcommand, Debug)]
pub enum SimCommand {
    /// Run a campaign across multiple seeds.
    #[command(
        about = "Run a simulation campaign across multiple seeds",
        long_about = "Execute deterministic simulation campaigns with configurable agent counts,\n\
                      rounds, fault injection, and seed ranges. Reports pass/fail per seed\n\
                      and identifies the first failure for replay.",
        after_help = "EXAMPLES:\n    # Run 100 seeds with defaults\n    bn sim run --seeds 100\n\n\
                      # Custom parameters\n    bn sim run --seeds 200 --agents 8 --rounds 32 --faults 0.2\n\n\
                      # Machine-readable output\n    bn sim run --seeds 100 --json"
    )]
    Run(SimRunArgs),

    /// Replay a single seed with full trace.
    #[command(
        about = "Replay a single seed with detailed trace output",
        long_about = "Replay a specific seed to get full execution trace, oracle results,\n\
                      and violation details. Use after a campaign failure to debug.",
        after_help = "EXAMPLES:\n    # Replay seed 42\n    bn sim replay --seed 42\n\n\
                      # Replay with custom parameters\n    bn sim replay --seed 42 --agents 8 --rounds 32\n\n\
                      # Machine-readable output\n    bn sim replay --seed 42 --json"
    )]
    Replay(SimReplayArgs),
}

/// Arguments for `bn sim run`.
#[derive(Args, Debug)]
pub struct SimRunArgs {
    /// Number of seeds to run (starting from 0).
    #[arg(long, default_value = "100")]
    pub seeds: u64,

    /// Starting seed value.
    #[arg(long, default_value = "0")]
    pub seed_start: u64,

    /// Number of simulated agents.
    #[arg(long, default_value = "5")]
    pub agents: usize,

    /// Number of simulation rounds per seed.
    #[arg(long, default_value = "24")]
    pub rounds: u64,

    /// Peers each emitter sends each event to.
    #[arg(long, default_value = "2")]
    pub fanout: usize,

    /// Overall fault probability (scales drop, dup, reorder, partition).
    /// Value between 0.0 and 1.0.
    #[arg(long, default_value = "0.1")]
    pub faults: f64,

    /// Maximum delivery delay in rounds.
    #[arg(long, default_value = "3")]
    pub max_delay: u8,
}

/// Arguments for `bn sim replay`.
#[derive(Args, Debug)]
pub struct SimReplayArgs {
    /// Seed to replay.
    #[arg(long)]
    pub seed: u64,

    /// Number of simulated agents.
    #[arg(long, default_value = "5")]
    pub agents: usize,

    /// Number of simulation rounds.
    #[arg(long, default_value = "24")]
    pub rounds: u64,

    /// Peers each emitter sends each event to.
    #[arg(long, default_value = "2")]
    pub fanout: usize,

    /// Overall fault probability (scales drop, dup, reorder, partition).
    #[arg(long, default_value = "0.1")]
    pub faults: f64,

    /// Maximum delivery delay in rounds.
    #[arg(long, default_value = "3")]
    pub max_delay: u8,
}

/// JSON output for `bn sim run`.
#[derive(Debug, Serialize)]
struct RunOutput {
    seeds_run: usize,
    seeds_passed: usize,
    seeds_failed: usize,
    first_failure: Option<u64>,
    interesting_states_reached: usize,
    all_passed: bool,
    failures: Vec<FailureOutput>,
}

#[derive(Debug, Serialize)]
struct FailureOutput {
    seed: u64,
    violations: Vec<String>,
}

/// JSON output for `bn sim replay`.
#[derive(Debug, Serialize)]
struct ReplayOutput {
    seed: u64,
    trace_events: usize,
    emitted_events: usize,
    agents: usize,
    converged: bool,
    oracle_passed: bool,
    violations: Vec<String>,
    interesting_state_reached: bool,
    trace_fingerprint: u64,
}

fn build_campaign_config(
    seed_start: u64,
    seeds: u64,
    agents: usize,
    rounds: u64,
    fanout: usize,
    faults: f64,
    max_delay: u8,
) -> bones_sim::campaign::CampaignConfig {
    // Scale individual fault rates from the overall fault probability.
    let drop = scale_fault(faults, 50); // drop is half of faults
    let dup = scale_fault(faults, 25); // dup is quarter
    let reorder = scale_fault(faults, 50);
    let partition = scale_fault(faults, 25);
    let freeze = scale_fault(faults, 25);

    bones_sim::campaign::CampaignConfig {
        seed_range: seed_start..(seed_start + seeds),
        agent_count: agents,
        rounds,
        fanout,
        fault_drop_percent: drop,
        fault_duplicate_percent: dup,
        fault_reorder_percent: reorder,
        fault_partition_percent: partition,
        fault_max_delay: max_delay,
        fault_freeze_percent: freeze,
        fault_freeze_duration: 2,
    }
}

/// Scale a base fault probability (0.0–1.0) by a weight to get a percent (0–100).
fn scale_fault(base: f64, weight_pct: u8) -> u8 {
    let raw = base * f64::from(weight_pct);
    let clamped = raw.clamp(0.0, 100.0);
    clamped as u8
}

/// Execute `bn sim run`.
pub fn run_sim_run(args: &SimRunArgs, output: OutputMode, _project_root: &Path) -> Result<()> {
    let config = build_campaign_config(
        args.seed_start,
        args.seeds,
        args.agents,
        args.rounds,
        args.fanout,
        args.faults,
        args.max_delay,
    );

    let report = bones_sim::campaign::run_campaign(&config)?;

    if output.is_json() {
        let out = RunOutput {
            seeds_run: report.seeds_run,
            seeds_passed: report.seeds_passed,
            seeds_failed: report.failures.len(),
            first_failure: report.first_failure,
            interesting_states_reached: report.interesting_states_reached,
            all_passed: report.all_passed(),
            failures: report
                .failures
                .iter()
                .map(|f| FailureOutput {
                    seed: f.seed,
                    violations: f.violations.clone(),
                })
                .collect(),
        };
        println!("{}", serde_json::to_string_pretty(&out)?);
    } else {
        println!(
            "Campaign: {} seeds, {} agents, {} rounds, faults={:.0}%",
            report.seeds_run,
            args.agents,
            args.rounds,
            args.faults * 100.0
        );
        println!(
            "Results: {}/{} passed, {} interesting states",
            report.seeds_passed, report.seeds_run, report.interesting_states_reached
        );

        if report.all_passed() {
            println!("✓ All seeds passed");
        } else {
            println!(
                "✗ {} failures (first at seed {})",
                report.failures.len(),
                report.first_failure.unwrap_or(0)
            );
            // Show first few failures
            for failure in report.failures.iter().take(5) {
                println!(
                    "  seed {}: {} violations",
                    failure.seed,
                    failure.violations.len()
                );
                for v in &failure.violations {
                    println!("    - {v}");
                }
            }
            if report.failures.len() > 5 {
                println!("  ... and {} more failures", report.failures.len() - 5);
            }
            println!(
                "\nReplay the first failure:\n  bn sim replay --seed {} --agents {} --rounds {}",
                report.first_failure.unwrap_or(0),
                args.agents,
                args.rounds
            );
        }
    }

    // Exit code 1 on any failure for CI integration
    if !report.all_passed() {
        process::exit(1);
    }

    Ok(())
}

/// Execute `bn sim replay`.
pub fn run_sim_replay(
    args: &SimReplayArgs,
    output: OutputMode,
    _project_root: &Path,
) -> Result<()> {
    let config = build_campaign_config(
        args.seed,
        1,
        args.agents,
        args.rounds,
        args.fanout,
        args.faults,
        args.max_delay,
    );

    let trace = bones_sim::campaign::replay_seed(args.seed, &config)?;

    if output.is_json() {
        let out = ReplayOutput {
            seed: args.seed,
            trace_events: trace.result.trace.len(),
            emitted_events: trace.all_events.len(),
            agents: trace.result.states.len(),
            converged: trace.result.convergence.converged,
            oracle_passed: trace.oracle.passed,
            violations: trace
                .oracle
                .violations
                .iter()
                .map(|v| format!("{v:?}"))
                .collect(),
            interesting_state_reached: trace.result.interesting_state_reached,
            trace_fingerprint: trace.result.trace_fingerprint(),
        };
        println!("{}", serde_json::to_string_pretty(&out)?);
    } else {
        println!("Replaying seed {}", args.seed);
        println!(
            "  Agents: {}, Rounds: {}, Fanout: {}",
            args.agents, args.rounds, args.fanout
        );
        println!(
            "  Trace events: {}, Emitted events: {}",
            trace.result.trace.len(),
            trace.all_events.len()
        );
        println!("  Converged: {}", trace.result.convergence.converged);
        println!(
            "  Interesting state reached: {}",
            trace.result.interesting_state_reached
        );
        println!(
            "  Trace fingerprint: {:016x}",
            trace.result.trace_fingerprint()
        );

        if trace.oracle.passed {
            println!("  ✓ All invariants passed");
        } else {
            println!(
                "  ✗ {} invariant violations:",
                trace.oracle.violations.len()
            );
            for v in &trace.oracle.violations {
                println!("    - {v:?}");
            }
        }

        // Show agent state summary
        for state in &trace.result.states {
            println!(
                "  Agent {}: {} known events",
                state.id,
                state.known_events.len()
            );
        }
    }

    if !trace.oracle.passed {
        process::exit(1);
    }

    Ok(())
}

/// Dispatch `bn sim` subcommands.
pub fn run_sim(args: &SimArgs, output: OutputMode, project_root: &Path) -> Result<()> {
    match &args.command {
        SimCommand::Run(run_args) => run_sim_run(run_args, output, project_root),
        SimCommand::Replay(replay_args) => run_sim_replay(replay_args, output, project_root),
    }
}
