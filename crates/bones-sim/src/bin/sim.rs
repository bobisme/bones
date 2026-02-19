#![forbid(unsafe_code)]

use anyhow::Result;
use bones_sim::{SimulationConfig, Simulator};

fn main() -> Result<()> {
    let mut simulator = Simulator::new(SimulationConfig::default())?;
    let result = simulator.run()?;

    println!(
        "simulation complete: trace_events={} converged={} interesting={}",
        result.trace.len(),
        result.convergence.converged,
        result.interesting_state_reached
    );

    Ok(())
}
