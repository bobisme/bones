use clap::Parser;
use tracing::info;

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    #[arg(short, long)]
    verbose: bool,
}

fn main() -> anyhow::Result<()> {
    // Initialize logging
    tracing_subscriber::fmt::init();

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
