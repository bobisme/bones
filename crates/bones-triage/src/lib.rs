#![forbid(unsafe_code)]
//! bones-triage library.
//!
//! # Conventions
//!
//! - **Errors**: Use `anyhow::Result` for return types.
//! - **Logging**: Use `tracing` macros (`info!`, `warn!`, `error!`, `debug!`, `trace!`).

pub mod feedback;
pub mod graph;
pub mod metrics;
pub mod schedule;
pub mod score;

use tracing::{info, instrument};

#[instrument]
pub fn init() {
    info!("bones-triage initialized");
}

#[cfg(test)]
mod tests {
    #[test]
    fn it_works() {
        assert!(true);
    }
}
