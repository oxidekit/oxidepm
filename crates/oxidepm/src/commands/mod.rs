//! Command implementations

pub mod adapter;
pub mod audit;
pub mod backup;
pub mod completions;
pub mod delete;
pub mod describe;
pub mod env_cmd;
pub mod flush;
pub mod kill;
pub mod logs;
pub mod ping;
pub mod policy;
pub mod restart;
pub mod resurrect;
pub mod save;
pub mod session;
pub mod show;
pub mod start;
pub mod startup;
pub mod status;
pub mod stop;
pub mod top;
pub mod update;
pub mod verb;

use oxidepm_core::constants;
use oxidepm_ipc::{IpcClient, Request, Response};

/// `--dry-run` was given: the commands that change something ask the daemon for a plan.
static DRY_RUN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub fn set_dry_run(on: bool) {
    DRY_RUN.store(on, std::sync::atomic::Ordering::SeqCst);
}

pub fn is_dry_run() -> bool {
    DRY_RUN.load(std::sync::atomic::Ordering::SeqCst)
}

/// The envelope options of a call: `reveal`, and `dry_run` when `--dry-run` was given.
pub(crate) fn call_options(reveal: bool) -> oxidepm_ipc::CallOptions {
    oxidepm_ipc::CallOptions {
        reveal,
        dry_run: is_dry_run(),
        ..Default::default()
    }
}

/// Get the IPC client: it talks to `OXIDEPM_SOCKET` (else the home's socket) and claims
/// `OXIDEPM_ACTOR`, if set, as its actor
pub fn get_client() -> IpcClient {
    IpcClient::new(constants::socket_path()).with_actor(constants::actor_claim())
}

/// Send a request to a running daemon. Never starts a daemon: a missing
/// daemon is reported as an error instead of being booted as a side effect.
pub async fn send(client: &IpcClient, request: &Request) -> anyhow::Result<Response> {
    client
        .send_with(request, &call_options(false))
        .await
        .map_err(daemon_error)
}

/// Like [`send`], optionally asking the daemon to reveal environment values and scripts.
pub async fn send_with(
    client: &IpcClient,
    request: &Request,
    reveal: bool,
) -> anyhow::Result<Response> {
    client
        .send_with(request, &call_options(reveal))
        .await
        .map_err(daemon_error)
}

/// Send a request, starting the daemon first if needed. Only for commands
/// that cannot do anything without a daemon (start, resurrect). A rehearsal never starts one:
/// without a daemon there is nothing to rehearse against.
pub async fn send_autostart(client: &IpcClient, request: &Request) -> anyhow::Result<Response> {
    if is_dry_run() {
        return send(client, request).await;
    }
    client.send_autostart(request).await.map_err(daemon_error)
}

/// If `response` is the answer to a rehearsal, show it and return what the command returns: Ok
/// for a plan the policy allows, an error for one it refuses. `None` for any other response.
pub fn plan_shown(response: &Response) -> Option<anyhow::Result<()>> {
    let Response::DryRun { plan } = response else {
        return None;
    };
    Some(crate::output::print_plan(plan))
}

fn daemon_error(e: oxidepm_core::Error) -> anyhow::Error {
    match e {
        oxidepm_core::Error::DaemonNotRunning => anyhow::anyhow!(
            "Daemon is not running (start a process with `oxidepm start` or run `oxidepm resurrect`)"
        ),
        other => other.into(),
    }
}
