//! Verbs that need the terminal (the monitor, an MCP server over stdio).
//!
//! A relayed verb prints what the daemon streams back. A session verb cannot: the adapter must
//! own the terminal (or the pipes of the program that started `oxidepm`). So the daemon starts the
//! adapter, under the same sandbox as every other adapter, and this process hands it its own
//! standard input and output as open descriptors, then waits. The CLI never executes an adapter
//! and never learns where one is installed.
//!
//! Before anything is sent the CLI checks that the socket it talks to belongs to the current user
//! and sits in a directory nobody else can write to (`OXIDEPM_SOCKET` can point anywhere), and
//! again that the session socket the daemon names is the same kind of socket, served by the same
//! user. While the session runs the CLI passes window size changes on, and a hangup, interrupt or
//! terminate ends the session; the terminal settings it found are put back however the session
//! ends.

use std::os::fd::AsRawFd;
use std::time::Duration;

use oxidepm_core::constants::socket_path;
use oxidepm_ipc::session::{check_private_socket, ServerLine, SessionClient};
use oxidepm_ipc::{AdapterReply, AdapterRequest, InvokeArgs, Request, Response};
use tokio::signal::unix::{signal, SignalKind};

/// How long the attach may take.
const ATTACH_TIMEOUT: Duration = Duration::from_secs(10);

/// The terminal settings found at the start, put back when this goes out of scope. A session
/// adapter that is killed cannot restore the terminal it put in raw mode; this does.
struct Terminal {
    saved: Option<libc::termios>,
}

impl Terminal {
    fn save() -> Terminal {
        // SAFETY: tcgetattr fills a termios the size of the struct we give it.
        let saved = unsafe {
            let mut settings: libc::termios = std::mem::zeroed();
            (libc::isatty(libc::STDIN_FILENO) == 1
                && libc::tcgetattr(libc::STDIN_FILENO, &mut settings) == 0)
                .then_some(settings)
        };
        Terminal { saved }
    }

    fn is_tty(&self) -> bool {
        self.saved.is_some()
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        if let Some(settings) = &self.saved {
            // SAFETY: restoring settings read from the same descriptor.
            unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, settings) };
        }
    }
}

/// Run the session verb `verb` and return its exit code.
pub async fn run(verb: &str, argv: &[String], dry_run: bool) -> anyhow::Result<i32> {
    if dry_run {
        anyhow::bail!("'oxidepm {verb}' is interactive and has no --dry-run. Nothing was run");
    }
    let cwd = std::env::current_dir()?.display().to_string();
    let client = super::get_client();
    if !client.is_daemon_running() {
        anyhow::bail!("Daemon is not running (start it with `oxidepm resurrect`, or start a process); verbs of adapters are run by the daemon");
    }
    let socket = socket_path();
    check_private_socket(&socket).map_err(|why| {
        anyhow::anyhow!(
            "will not hand your terminal to the daemon at {}: {why}",
            socket.display()
        )
    })?;

    let request = Request::Adapter(Box::new(AdapterRequest::SessionOpen(Box::new(
        InvokeArgs {
            verb: verb.to_string(),
            argv: argv.to_vec(),
            cwd,
        },
    ))));
    let offer = match super::send(&client, &request).await? {
        Response::Adapter(reply) => match *reply {
            AdapterReply::Session(offer) => offer,
            _ => anyhow::bail!("Unexpected response from the daemon"),
        },
        Response::Error { code, message } => {
            return Err(super::adapter::coded(code.as_str(), message))
        }
        _ => anyhow::bail!("Unexpected response from the daemon"),
    };

    // before the adapter can change anything
    let terminal = Terminal::save();
    let mut winch = signal(SignalKind::window_change())?;
    let mut interrupt = signal(SignalKind::interrupt())?;
    let mut terminate = signal(SignalKind::terminate())?;
    let mut hangup = signal(SignalKind::hangup())?;

    let attached = SessionClient::attach(
        &offer,
        terminal.is_tty(),
        std::io::stdin().as_raw_fd(),
        std::io::stdout().as_raw_fd(),
        ATTACH_TIMEOUT,
    )
    .await
    .map_err(|why| anyhow::anyhow!("{why}"))?;
    let (mut events, mut control) = attached.split();

    let code = loop {
        tokio::select! {
            line = events.next() => match line {
                Ok(Some(ServerLine::Done { exit_code, .. })) => break exit_code,
                Ok(Some(ServerLine::Error { code, message })) => return Err(super::adapter::coded(&code, message)),
                Ok(None) => anyhow::bail!("The daemon ended the session without saying how"),
                Err(e) => anyhow::bail!("The session connection failed: {e}"),
            },
            _ = winch.recv() => {
                // a failure to tell the daemon means it is gone; the next read says so
                let _ = control.winch().await;
            }
            _ = interrupt.recv() => break 130,
            _ = terminate.recv() => break 143,
            _ = hangup.recv() => break 129,
        }
    };
    // leaving closes the connection, which ends the session; the terminal is put back first
    drop(control);
    drop(terminal);
    Ok(code)
}
