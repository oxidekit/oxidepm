//! OxidePM CLI - Process manager written in Rust for any runtime, with adapter support

use anyhow::Result;
use clap::Parser;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

mod cli;
mod commands;
mod output;

use cli::{Cli, Commands, DryRun, PolicyCommands};
use commands::*;

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize logging based on verbosity
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        // a command clap does not know may be a verb of an adapter
        Err(e) if e.kind() == clap::error::ErrorKind::InvalidSubcommand => {
            commands::verb::fallback(e).await
        }
        Err(e) => {
            if let Some(hint) = cli::removed_start_flag_hint(&e) {
                let _ = e.print();
                eprintln!("\n{hint}");
                std::process::exit(2);
            }
            e.exit();
        }
    };

    // Set JSON output mode if requested
    output::set_json_mode(cli.json);

    // --dry-run is refused where it cannot be honoured: a flag that is silently ignored would
    // run the command for real
    if cli.dry_run {
        let name = cli.command.name();
        let refusal = match cli.command.dry_run() {
            DryRun::Plan | DryRun::Own => None,
            DryRun::ReadOnly => Some(format!(
                "'oxidepm {name}' only reads, so --dry-run has nothing to rehearse. Run it without --dry-run"
            )),
            DryRun::None => Some(format!(
                "'oxidepm {name}' has no --dry-run yet. Nothing was run"
            )),
        };
        if let Some(message) = refusal {
            eprintln!("Error: {message}");
            std::process::exit(2);
        }
    }
    let dry_run = cli.dry_run;
    commands::set_dry_run(dry_run);

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
        .with(
            // Logs go to stderr so stdout stays clean for --json and pipes.
            tracing_subscriber::fmt::layer()
                .without_time()
                .with_writer(std::io::stderr)
                .with_ansi(atty::is(atty::Stream::Stderr)),
        )
        .init();

    // Handle commands
    let result = match cli.command {
        Commands::Start(args) => start::execute(args).await,
        Commands::Stop { selector } => stop::execute(&selector).await,
        Commands::Restart { selector } => restart::execute(&selector).await,
        Commands::Delete { selector } => delete::execute(&selector).await,
        Commands::Status { more, daemon } => {
            if daemon {
                status::execute_daemon().await
            } else {
                status::execute(more).await
            }
        }
        Commands::Policy { command } => match command {
            PolicyCommands::Set {
                selector,
                protected,
                ai_control,
            } => policy::execute_set(&selector, protected, ai_control).await,
        },
        Commands::Show { selector, reveal } => show::execute(&selector, reveal).await,
        Commands::Logs(args) => logs::execute(args).await,
        Commands::Audit(args) => audit::execute(args),
        Commands::Ping => ping::execute().await,
        Commands::Save { force } => save::execute(force).await,
        Commands::Resurrect => resurrect::execute().await,
        Commands::Kill => kill::execute().await,
        Commands::Startup { target } => startup::execute(target, dry_run),
        Commands::Reload { selector } => restart::execute(&selector).await, // reload is a plain restart
        Commands::Flush { selector } => flush::execute(&selector).await,
        Commands::Describe { target } => describe::execute(&target).await,
        Commands::Update { version, force } => update::execute(version, force, dry_run).await,
        Commands::Env(args) => env_cmd::execute(args).await,
        Commands::Top => top::execute().await,
        Commands::Completions { shell } => {
            completions::execute_completions(&shell)?;
            Ok(())
        }
        Commands::ManPage => {
            completions::execute_manpage()?;
            Ok(())
        }
        Commands::Backup { output } => backup::execute_backup(output, dry_run).await,
        Commands::Restore { file, yes } => backup::execute_restore(&file, yes, dry_run).await,
        Commands::Adapter(args) => adapter::execute(args, dry_run).await,
    };

    if let Err(e) = result {
        // a refusal that asks the user for something (a hash to pin, a fingerprint to pass)
        if let Some(refusal) = e.downcast_ref::<adapter::Refusal>() {
            eprintln!("Error: {refusal}");
            std::process::exit(2);
        }
        eprintln!("Error: {:#}", e);
        std::process::exit(1);
    }

    Ok(())
}
