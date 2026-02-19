#![forbid(unsafe_code)]
use clap::Parser;
use std::env;
use tracing::info;
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    #[arg(short, long)]
    verbose: bool,
}

fn init_tracing() {
    let filter = EnvFilter::try_from_env("BONES_LOG").unwrap_or_else(|_| {
        EnvFilter::new(if env::var("DEBUG").is_ok() {
            "bones=debug,info"
        } else {
            "bones=info,warn"
        })
    });

    let format = env::var("BONES_LOG_FORMAT").unwrap_or_else(|_| "compact".to_string());

    let registry = tracing_subscriber::registry().with(filter);

    match format.as_str() {
        "json" => {
            registry
                .with(fmt::layer().json().with_ansi(false))
                .init();
        }
        _ => {
            registry
                .with(fmt::layer().compact())
                .init();
        }
    }
}

fn main() -> anyhow::Result<()> {
    // Initialize logging
    init_tracing();

    let args = Args::parse();

    if args.verbose {
        info!("Verbose mode enabled");
    }

    info!("bones-cli initialized");

    // Initialize core systems (demonstration)
    bones_core::init();
    bones_triage::init();
    bones_search::init();
    bones_sim::init();

    Ok(())
}
