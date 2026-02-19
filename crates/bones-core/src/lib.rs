#![forbid(unsafe_code)]
//! bones-core library.

pub mod clock;
pub mod crdt;
pub mod error;
pub mod lock;
pub mod model;

use tracing::{info, instrument};

/// # Conventions
///
/// - **Errors**: Use `anyhow::Result` for return types where appropriate.
/// - **Logging**: Use `tracing` macros (`info!`, `warn!`, `error!`, `debug!`, `trace!`).

#[instrument]
pub fn init() {
    info!("bones-core initialized");
    // Ensure .gitattributes exists and has the union merge driver for events
    let gitattributes_path = std::path::Path::new(".gitattributes");
    let attr_line = ".bones/events merge=union\n";

    let mut content = if gitattributes_path.exists() {
        std::fs::read_to_string(gitattributes_path).unwrap_or_default()
    } else {
        String::new()
    };

    if !content.contains(".bones/events merge=union") {
        if !content.is_empty() && !content.ends_with('\n') {
            content.push('\n');
        }
        content.push_str(attr_line);
        let _ = std::fs::write(gitattributes_path, content);
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn it_works() {
        assert!(true);
    }
}
