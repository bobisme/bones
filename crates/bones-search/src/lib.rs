#![forbid(unsafe_code)]
//! bones-search library.
//!
//! # Conventions
//!
//! - **Errors**: Use `anyhow::Result` for return types.
//! - **Logging**: Use `tracing` macros (`info!`, `warn!`, `error!`, `debug!`, `trace!`).

pub mod semantic;

use tracing::{info, instrument};

#[instrument]
pub fn init() {
    info!("bones-search initialized");
}

#[cfg(test)]
mod tests {
    #[test]
    fn it_works() {
        assert!(true);
    }
}
