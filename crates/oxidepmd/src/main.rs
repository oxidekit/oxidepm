//! OxidePM Daemon - Process supervisor

#![deny(clippy::await_holding_lock, clippy::await_holding_refcell_ref)]

use std::io::Write;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::process::ExitCode;
use std::sync::Arc;

use anyhow::Result;
use oxidepm_core::{Paths, EXIT_ALREADY_RUNNING};
use tracing::{info, warn};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, Layer};

use oxidepmd::daemon::{check_state, Daemon, RunExit};

fn main() -> ExitCode {
    let mut args = std::env::args_os().skip(1);
    match (args.next(), args.next()) {
        (None, None) => run_daemon(false),
        (Some(arg), None) if arg == "--check" => run_daemon(true),
        (Some(arg), None) if arg == "--version" || arg == "-V" => {
            println!("oxidepmd {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        (Some(arg), None) if arg == "--help" || arg == "-h" => {
            println!(concat!(
                "Usage: oxidepmd [OPTION]\n\n",
                "Run the daemon when no option is given.\n\n",
                "  --check         Check the daemon state\n",
                "  --version, -V   Print the version\n",
                "  --help, -h      Print this help"
            ));
            ExitCode::SUCCESS
        }
        (first, second) => {
            // name the first argument that is not an option; with two options, the second is extra
            let known = ["--check", "--version", "-V", "--help", "-h"];
            let arg = match (first, second) {
                (Some(first), Some(second)) if known.iter().any(|k| first == *k) => second,
                (Some(arg), _) | (None, Some(arg)) => arg,
                (None, None) => unreachable!("no arguments run the daemon"),
            };
            eprintln!("oxidepmd: unknown argument '{}'", arg.to_string_lossy());
            eprintln!("Run 'oxidepmd --help' for usage.");
            ExitCode::from(2)
        }
    }
}

#[tokio::main]
async fn run_daemon(check: bool) -> ExitCode {
    let paths = match Paths::from_env() {
        Ok(paths) => Arc::new(paths),
        Err(e) => {
            eprintln!("oxidepmd: {e}");
            return ExitCode::FAILURE;
        }
    };

    if check {
        return match check_state(&paths).await {
            Ok(()) => {
                println!("oxidepmd: state in {} is usable", paths.home().display());
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("oxidepmd: {e}");
                ExitCode::FAILURE
            }
        };
    }

    // The log file lives in the home, which must exist (private) before the first line is logged.
    if let Err(e) = paths.ensure_dirs() {
        report_fatal(&paths, &format!("oxidepmd: {e}"));
        return ExitCode::FAILURE;
    }

    // Diagnostics go to the daemon log file in the home (and to stderr, unless that already is
    // the daemon log). The guard flushes the file when `main` returns.
    let _log_guard = init_logging(&paths);

    match run(Arc::clone(&paths)).await {
        Ok(RunExit::Clean) => ExitCode::SUCCESS,
        // a second signal cut the shutdown short
        Ok(RunExit::Forced) => ExitCode::from(130),
        Err(e) if is_already_running(&e) => {
            // Not a failure of this daemon: the one that holds the lock is serving. A client
            // that started us just waits for its socket.
            eprintln!(
                "oxidepmd is already running (lock held: {})",
                paths.lock().display()
            );
            ExitCode::from(EXIT_ALREADY_RUNNING as u8)
        }
        Err(e) => {
            report_fatal(&paths, &format!("oxidepmd: {e:#}"));
            ExitCode::FAILURE
        }
    }
}

async fn run(paths: Arc<Paths>) -> Result<RunExit> {
    info!("OxidePM Daemon starting...");

    // Lock, database, supervisor, socket, then the saved apps in the background; signals are
    // handled from the first moment (see `Daemon::with_paths`).
    let daemon = Daemon::with_paths(paths).await?;
    let exit = daemon.run().await?;
    info!("Daemon shutdown complete");
    Ok(exit)
}

/// Log to `{home}/daemon.log.<date>` (daily rolling, the last 7 days kept, written off the
/// daemon's threads) and to stderr, except when stderr is the `daemon.log` file a client
/// redirected it to (the two would only duplicate each other).
fn init_logging(paths: &Paths) -> Option<tracing_appender::non_blocking::WorkerGuard> {
    type BoxedLayer = Box<dyn Layer<tracing_subscriber::Registry> + Send + Sync>;
    let filter = || {
        tracing_subscriber::EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| "oxidepmd=info,oxidepm_db=info".into())
    };

    let mut layers: Vec<BoxedLayer> = Vec::new();
    if !stderr_is_daemon_log(paths) {
        layers.push(
            tracing_subscriber::fmt::layer()
                .with_writer(std::io::stderr)
                .with_filter(filter())
                .boxed(),
        );
    }

    let appender = tracing_appender::rolling::Builder::new()
        .rotation(tracing_appender::rolling::Rotation::DAILY)
        .filename_prefix("daemon.log")
        .max_log_files(7)
        .build(paths.home());

    let mut guard = None;
    let mut file_error = None;
    match appender {
        Ok(appender) => {
            private_daemon_logs(paths);
            let (writer, worker_guard) = tracing_appender::non_blocking(appender);
            layers.push(
                tracing_subscriber::fmt::layer()
                    .with_ansi(false)
                    .with_writer(writer)
                    .with_filter(filter())
                    .boxed(),
            );
            guard = Some(worker_guard);
        }
        Err(e) => file_error = Some(e),
    }

    tracing_subscriber::registry().with(layers).init();
    if let Some(e) = file_error {
        warn!(
            "Cannot open the daemon log in {}: {}",
            paths.home().display(),
            e
        );
    }
    guard
}

/// Make the daemon log files owner-only. The appender creates them with the default mode, and
/// the home directory is 0700 anyway; this is the belt to that pair of braces.
fn private_daemon_logs(paths: &Paths) {
    use std::os::unix::fs::PermissionsExt;
    let Ok(entries) = std::fs::read_dir(paths.home()) else {
        return;
    };
    for entry in entries.flatten() {
        if entry
            .file_name()
            .to_string_lossy()
            .starts_with("daemon.log.")
        {
            let _ = std::fs::set_permissions(entry.path(), std::fs::Permissions::from_mode(0o600));
        }
    }
}

fn is_already_running(error: &anyhow::Error) -> bool {
    matches!(
        error.downcast_ref::<oxidepm_core::Error>(),
        Some(oxidepm_core::Error::DaemonAlreadyRunning)
    )
}

/// A fatal startup error goes to stderr and to `{home}/daemon.log`: an auto-started daemon
/// has no terminal, and the client shows the tail of that log when the daemon exits early.
fn report_fatal(paths: &Paths, message: &str) {
    eprintln!("{message}");
    if stderr_is_daemon_log(paths) {
        return; // the client already redirected stderr there; do not write it twice
    }
    let appended = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(paths.daemon_log())
        .and_then(|mut f| writeln!(f, "{message}"));
    if let Err(e) = appended {
        eprintln!(
            "oxidepmd: cannot write {}: {e}",
            paths.daemon_log().display()
        );
    }
}

/// True when this process's stderr is the daemon log file itself (Linux: `/proc/self/fd/2`;
/// elsewhere the answer is "no" and the message may appear twice).
fn stderr_is_daemon_log(paths: &Paths) -> bool {
    match (
        std::fs::metadata("/proc/self/fd/2"),
        std::fs::metadata(paths.daemon_log()),
    ) {
        (Ok(a), Ok(b)) => a.dev() == b.dev() && a.ino() == b.ino(),
        _ => false,
    }
}
