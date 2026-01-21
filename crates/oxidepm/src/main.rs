//! OxidePM CLI - PM2-like process manager for Rust and Node.js

use anyhow::Result;
use clap::Parser;
use oxidepm_core::constants::socket_path;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

mod cli;
mod commands;
mod output;

use cli::{Cli, Commands};
use commands::*;

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize logging based on verbosity
    let cli = Cli::parse();

    // Set JSON output mode if requested
    output::set_json_mode(cli.json);

    let log_level = match cli.verbose {
        0 => "warn",
        1 => "info",
        2 => "debug",
        _ => "trace",
    };

    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| format!("oxidepm={}", log_level).into()),
        )
        .with(tracing_subscriber::fmt::layer().without_time())
        .init();

    // Handle commands
    let result = match cli.command {
        Commands::Start(args) => start::execute(args).await,
        Commands::Stop { selector } => stop::execute(&selector).await,
        Commands::Restart { selector } => restart::execute(&selector).await,
        Commands::Delete { selector } => delete::execute(&selector).await,
        Commands::Status { more } => status::execute(more).await,
        Commands::Show { selector } => show::execute(&selector).await,
        Commands::Logs(args) => logs::execute(args).await,
        Commands::Ping => ping::execute().await,
        Commands::Save => save::execute().await,
        Commands::Resurrect => resurrect::execute().await,
        Commands::Kill => kill::execute().await,
        Commands::Startup { target } => startup::execute(target),
        Commands::Monit => {
            oxidepm_tui::run(socket_path()).await.map_err(|e| anyhow::anyhow!(e))
        }
        Commands::Web(args) => {
            let bind_addr = format!("0.0.0.0:{}", args.port);
            oxidepm_web::start_server(&bind_addr, socket_path(), args.api_key)
                .await
                .map_err(|e| anyhow::anyhow!(e))
        }
        Commands::Reload { selector } => restart::execute(&selector).await, // Graceful restart uses same logic
        Commands::Notify(args) => notify::execute(args).await,
        Commands::Flush { selector } => flush::execute(&selector).await,
        Commands::Describe { target } => describe::execute(&target).await,
        Commands::Check(args) => check::execute(args).await,
    };

    if let Err(e) = result {
        eprintln!("Error: {}", e);
        std::process::exit(1);
    }

    Ok(())
}
