//! AWS Lambda entry point; configuration can be supplied entirely by environment.
use clap::Parser;

#[derive(Parser)]
#[command(name = "kimetsu-remote-lambda", version)]
struct Cli {
    #[command(flatten)]
    args: kimetsu_remote::config::ServeArgs,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    kimetsu_remote::run_lambda(Cli::parse().args)
        .await
        .map_err(Into::into)
}
