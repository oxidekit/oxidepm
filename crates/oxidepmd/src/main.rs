//! OxidePM Daemon - Process supervisor

use anyhow::Result;
use oxidepm_core::constants;
use tracing::{error, info};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

mod daemon;
mod handlers;
mod supervisor;

use daemon::Daemon;

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize logging
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "oxidepmd=info,oxidepm_db=info".into()),
        )
        .with(tracing_subscriber::fmt::layer())
        .init();

    info!("OxidePM Daemon starting...");

    // Ensure home directory exists
    let home = constants::oxidepm_home();
    if !home.exists() {
        std::fs::create_dir_all(&home)?;
        info!("Created OxidePM home directory: {}", home.display());
    }

    // Check if daemon is already running
    let socket_path = constants::socket_path();
    if socket_path.exists() {
        // Try to connect to see if it's a stale socket
        match tokio::net::UnixStream::connect(&socket_path).await {
            Ok(_) => {
                error!("Daemon is already running");
                std::process::exit(1);
            }
            Err(_) => {
                // Stale socket, remove it
                info!("Removing stale socket file");
                std::fs::remove_file(&socket_path)?;
            }
        }
    }

    // Create and run daemon
    let daemon = Daemon::new().await?;

    // Set up signal handlers
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let mut sigint = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;

    tokio::select! {
        result = daemon.run() => {
            if let Err(e) = result {
                error!("Daemon error: {}", e);
                return Err(e.into());
            }
        }
        _ = sigterm.recv() => {
            info!("Received SIGTERM, shutting down...");
        }
        _ = sigint.recv() => {
            info!("Received SIGINT, shutting down...");
        }
    }

    info!("Daemon shutdown complete");
    Ok(())
}
