#![forbid(unsafe_code)]
//! bones-core library.

pub mod crdt;
pub mod lock;

/// # Conventions
///
/// - **Errors**: Use `anyhow::Result` for return types where appropriate.
/// - **Logging**: Use `tracing` macros (`info!`, `warn!`, `error!`, `debug!`, `trace!`).

pub fn init() {
    tracing::info!("bones-core initialized");
}

#[cfg(test)]
mod tests {
    #[test]
    fn it_works() {
        assert!(true);
    }
}
