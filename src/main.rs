use anyhow::Result;
use clap::Parser;
use obsidian_cli::cli::{self, Cli};

#[tokio::main]
async fn main() -> Result<()> {
    let parsed = Cli::parse();
    cli::run(parsed).await
}
