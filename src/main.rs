//! orbis entry point: parse arguments, set up logging, then run the GUI or
//! a CLI command.

use std::path::PathBuf;

use anyhow::Result;
use clap::Parser;

mod cli;
mod gui;

/// KSP mod manager with Prism-style instances, built on CKAN metadata.
#[derive(Parser, Debug)]
#[command(name = "orbis", version, about)]
struct Args {
    /// Keep config, data and cache under this directory instead of the XDG
    /// locations.
    #[arg(long, global = true, value_name = "DIR")]
    root: Option<PathBuf>,

    #[command(subcommand)]
    command: Option<cli::Command>,
}

fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();
    let args = Args::parse();
    let ctx = cli::Context::new(args.root.as_deref())?;
    let result = match args.command {
        None => gui::run(ctx, gui::Options::default()),
        Some(cli::Command::Gui(opts)) => gui::run(ctx, opts),
        Some(cmd) => cli::run(&ctx, cmd),
    };
    match result {
        // Output piped into something that stopped reading (`| head`).
        Err(e) if broken_pipe(&e) => Ok(()),
        other => other,
    }
}

fn broken_pipe(e: &anyhow::Error) -> bool {
    e.chain().any(|c| {
        c.downcast_ref::<std::io::Error>()
            .is_some_and(|io| io.kind() == std::io::ErrorKind::BrokenPipe)
    })
}
