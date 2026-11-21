use anyhow::Result;
use obsidian_cli::cli::{self, Cli};

#[tokio::main]
async fn main() -> Result<()> {
    let parsed = Cli::parse_args();
    cli::run(parsed).await
}
