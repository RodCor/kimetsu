//! `kimetsu-remote` — a server that exposes the Kimetsu brain over HTTP MCP,
//! one brain per repository under a data dir. Pure-DB tools only; bearer auth;
//! plain HTTP (terminate TLS at a reverse proxy). Thin wrapper over the lib.

use std::process::ExitCode;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "kimetsu-remote",
    version,
    about = "Server-hosted Kimetsu brain over HTTP MCP (repo-keyed)."
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Serve the HTTP MCP brain.
    Serve(kimetsu_remote::config::ServeArgs),
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Serve(args) => match kimetsu_remote::run_serve(args) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("kimetsu-remote: {e}");
                ExitCode::FAILURE
            }
        },
    }
}
