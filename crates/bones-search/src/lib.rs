#![forbid(unsafe_code)]
//! bones-search library.
//!
//! # Conventions
//!
//! - **Errors**: Use `anyhow::Result` for return types.
//! - **Logging**: Use `tracing` macros (`info!`, `warn!`, `error!`, `debug!`, `trace!`).

pub fn init() {
    tracing::info!("bones-search initialized");
}

#[cfg(test)]
mod tests {
    #[test]
    fn it_works() {
        assert!(true);
    }
}
