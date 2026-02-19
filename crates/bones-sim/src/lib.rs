//! bones-sim library.
//!
//! # Conventions
//!
//! - **Errors**: Use `anyhow::Result` for return types.
//! - **Logging**: Use `tracing` macros (`info!`, `warn!`, `error!`, `debug!`, `trace!`).

pub fn init() {
    tracing::info!("bones-sim initialized");
}

#[cfg(test)]
mod tests {
    #[test]
    fn it_works() {
        assert!(true);
    }
}
