//! `bn dev sim` — deterministic simulation campaign commands.
//!
//! `bn dev sim run` — execute a campaign across many seeds.
//! `bn dev sim replay` — replay a single seed with detailed trace output.

use std::path::Path;
use std::process;

use anyhow::Result;
use clap::{Args, Subcommand};
use serde::Serialize;

use crate::output::{OutputMode, pretty_kv, pretty_section};

/// Top-level arguments for `bn dev sim`.
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
        after_help = "EXAMPLES:\n    # Run 100 seeds with defaults\n    bn dev sim run --seeds 100\n\n\
                      # Custom parameters\n    bn dev sim run --seeds 200 --agents 8 --rounds 32 --faults 0.2\n\n\
                      # Machine-readable output\n    bn dev sim run --seeds 100 --format json"
    )]
    Run(SimRunArgs),

    /// Replay a single seed with full trace.
    #[command(
        about = "Replay a single seed with detailed trace output",
        long_about = "Replay a specific seed to get full execution trace, oracle results,\n\
                      and violation details. Use after a campaign failure to debug.",
        after_help = "EXAMPLES:\n    # Replay seed 42\n    bn dev sim replay --seed 42\n\n\
                      # Replay with custom parameters\n    bn dev sim replay --seed 42 --agents 8 --rounds 32\n\n\
                      # Machine-readable output\n    bn dev sim replay --seed 42 --format json"
    )]
    Replay(SimReplayArgs),
}

/// Arguments for `bn dev sim run`.
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

/// Arguments for `bn dev sim replay`.
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

/// JSON output for `bn dev sim run`.
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

/// JSON output for `bn dev sim replay`.
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

/// Execute `bn dev sim run`.
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

    match output {
        OutputMode::Json => {
            println!("{}", serde_json::to_string_pretty(&out)?);
        }
        OutputMode::Text => {
            println!(
                "campaign seeds_run={} agents={} rounds={} faults_pct={:.0}",
                out.seeds_run,
                args.agents,
                args.rounds,
                args.faults * 100.0
            );
            println!(
                "results passed={} failed={} interesting_states={} all_passed={}",
                out.seeds_passed, out.seeds_failed, out.interesting_states_reached, out.all_passed
            );
            if !out.all_passed {
                for failure in out.failures.iter().take(5) {
                    println!(
                        "failure seed={} violations={}",
                        failure.seed,
                        failure.violations.len()
                    );
                }
                if out.failures.len() > 5 {
                    println!("failures_truncated count={}", out.failures.len() - 5);
                }
                println!(
                    "hint replay_seed={} agents={} rounds={}",
                    out.first_failure.unwrap_or(0),
                    args.agents,
                    args.rounds
                );
            }
        }
        OutputMode::Pretty => {
            let stdout = std::io::stdout();
            let mut w = stdout.lock();
            pretty_section(&mut w, "Simulation Campaign")?;
            pretty_kv(&mut w, "Seeds", out.seeds_run.to_string())?;
            pretty_kv(&mut w, "Agents", args.agents.to_string())?;
            pretty_kv(&mut w, "Rounds", args.rounds.to_string())?;
            pretty_kv(&mut w, "Fault rate", format!("{:.0}%", args.faults * 100.0))?;
            pretty_kv(
                &mut w,
                "Results",
                format!(
                    "{} passed / {} failed ({} interesting states)",
                    out.seeds_passed, out.seeds_failed, out.interesting_states_reached
                ),
            )?;

            if out.all_passed {
                pretty_kv(&mut w, "Status", "all seeds passed")?;
            } else {
                pretty_kv(
                    &mut w,
                    "Status",
                    format!(
                        "{} failures (first at seed {})",
                        out.seeds_failed,
                        out.first_failure.unwrap_or(0)
                    ),
                )?;
                println!();
                pretty_section(&mut w, "Failure Samples")?;
                for failure in out.failures.iter().take(5) {
                    println!(
                        "seed {:<8} violations={} ",
                        failure.seed,
                        failure.violations.len()
                    );
                    for violation in &failure.violations {
                        println!("  - {violation}");
                    }
                }
                if out.failures.len() > 5 {
                    println!("... and {} more failures", out.failures.len() - 5);
                }
                println!();
                pretty_kv(
                    &mut w,
                    "Replay",
                    format!(
                        "bn dev sim replay --seed {} --agents {} --rounds {}",
                        out.first_failure.unwrap_or(0),
                        args.agents,
                        args.rounds
                    ),
                )?;
            }
        }
    }

    // Exit code 1 on any failure for CI integration
    if !report.all_passed() {
        process::exit(1);
    }

    Ok(())
}

/// Execute `bn dev sim replay`.
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

    match output {
        OutputMode::Json => {
            println!("{}", serde_json::to_string_pretty(&out)?);
        }
        OutputMode::Text => {
            println!(
                "replay seed={} agents={} rounds={} fanout={}",
                out.seed, args.agents, args.rounds, args.fanout
            );
            println!(
                "result converged={} oracle_passed={} trace_events={} emitted_events={} interesting_state_reached={} trace_fingerprint={:016x}",
                out.converged,
                out.oracle_passed,
                out.trace_events,
                out.emitted_events,
                out.interesting_state_reached,
                out.trace_fingerprint
            );
            for violation in &out.violations {
                println!("violation={violation}");
            }
            for state in &trace.result.states {
                println!(
                    "agent id={} known_events={}",
                    state.id,
                    state.known_events.len()
                );
            }
        }
        OutputMode::Pretty => {
            let stdout = std::io::stdout();
            let mut w = stdout.lock();
            pretty_section(&mut w, &format!("Replay Seed {}", out.seed))?;
            pretty_kv(&mut w, "Agents", args.agents.to_string())?;
            pretty_kv(&mut w, "Rounds", args.rounds.to_string())?;
            pretty_kv(&mut w, "Fanout", args.fanout.to_string())?;
            pretty_kv(&mut w, "Trace events", out.trace_events.to_string())?;
            pretty_kv(&mut w, "Emitted", out.emitted_events.to_string())?;
            pretty_kv(&mut w, "Converged", out.converged.to_string())?;
            pretty_kv(
                &mut w,
                "Interesting",
                out.interesting_state_reached.to_string(),
            )?;
            pretty_kv(
                &mut w,
                "Fingerprint",
                format!("{:016x}", out.trace_fingerprint),
            )?;
            pretty_kv(&mut w, "Oracle", out.oracle_passed.to_string())?;

            if !out.oracle_passed {
                println!();
                pretty_section(&mut w, "Invariant Violations")?;
                for violation in &out.violations {
                    println!("- {violation}");
                }
            }

            println!();
            pretty_section(&mut w, "Agent States")?;
            for state in &trace.result.states {
                println!(
                    "agent {:<8} known_events={}",
                    state.id,
                    state.known_events.len()
                );
            }
        }
    }

    if !trace.oracle.passed {
        process::exit(1);
    }

    Ok(())
}

/// Dispatch `bn dev sim` subcommands.
pub fn run_sim(args: &SimArgs, output: OutputMode, project_root: &Path) -> Result<()> {
    match &args.command {
        SimCommand::Run(run_args) => run_sim_run(run_args, output, project_root),
        SimCommand::Replay(replay_args) => run_sim_replay(replay_args, output, project_root),
    }
}
