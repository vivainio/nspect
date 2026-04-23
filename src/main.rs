use anyhow::Result;
use clap::Parser;
use nspect::cli::{Cli, Command};

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();
    match cli.command {
        Command::Scan(args) => nspect::cli::run_scan(args),
        Command::Graph(args) => nspect::cli::run_graph(args),
        Command::Check(args) => nspect::cli::run_check(args),
        Command::TsDump(args) => nspect::cli::run_ts_dump(args),
        Command::Atlas(args) => nspect::cli::run_atlas(args),
        Command::Focus(args) => nspect::cli::run_focus(args),
    }
}
