//! Shared helpers for CLI integration tests.
//!
//! Every test runs the real `oxidepm` binary with `HOME` pointing at a temp
//! directory, so nothing here ever touches the real `~/.oxidepm`.
#![allow(dead_code)]

use assert_cmd::Command;
use oxidepm_ipc::{Envelope, Request, Response, ResponseEnvelope};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;
use tempfile::TempDir;

/// An isolated HOME directory.
pub struct TestHome {
    pub dir: TempDir,
}

impl TestHome {
    pub fn new() -> Self {
        Self {
            dir: tempfile::tempdir().expect("tempdir"),
        }
    }

    pub fn path(&self) -> &Path {
        self.dir.path()
    }

    pub fn oxidepm_dir(&self) -> PathBuf {
        let d = self.path().join(".oxidepm");
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    pub fn socket(&self) -> PathBuf {
        self.path().join(".oxidepm").join("daemon.sock")
    }

    /// `oxidepm` command bound to this fixture, with a hard timeout.
    pub fn cmd(&self) -> Command {
        let mut cmd = Command::cargo_bin("oxidepm").expect("oxidepm binary");
        cmd.env("HOME", self.path())
            .env("OXIDEPM_HOME", self.path().join(".oxidepm"))
            .env_remove("RUST_LOG")
            .env_remove("SUDO_USER")
            .current_dir(self.path())
            .timeout(Duration::from_secs(30));
        cmd
    }
}

type Handler = dyn Fn(&Request) -> Option<Response> + Send + 'static;

/// Minimal in-process stand-in for `oxidepmd`: listens on `$HOME/.oxidepm/daemon.sock`
/// and answers each request through `handler` (`None` closes the connection without a reply).
pub struct MockDaemon {
    pub requests: Arc<Mutex<Vec<Request>>>,
    /// The envelopes the requests arrived in (request id, reveal flag, ...).
    pub envelopes: Arc<Mutex<Vec<Envelope>>>,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl MockDaemon {
    pub fn start<F>(home: &TestHome, handler: F) -> Self
    where
        F: Fn(&Request) -> Option<Response> + Send + 'static,
    {
        home.oxidepm_dir();
        Self::start_at(&home.socket(), handler)
    }

    /// Like [`start`](Self::start), listening on the socket at `socket`.
    pub fn start_at<F>(socket: &Path, handler: F) -> Self
    where
        F: Fn(&Request) -> Option<Response> + Send + 'static,
    {
        let listener = UnixListener::bind(socket).expect("bind mock socket");
        listener.set_nonblocking(true).unwrap();

        let requests = Arc::new(Mutex::new(Vec::new()));
        let envelopes = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let handler: Box<Handler> = Box::new(handler);

        let (reqs, envs, stop2) = (requests.clone(), envelopes.clone(), stop.clone());
        let handle = std::thread::spawn(move || {
            while !stop2.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        stream.set_nonblocking(false).unwrap();
                        if let Err(error) = stream.set_read_timeout(Some(Duration::from_secs(5))) {
                            // macOS rejects socket options after a probe disconnects.
                            if cfg!(target_os = "macos")
                                && error.kind() == std::io::ErrorKind::InvalidInput
                            {
                                continue;
                            }
                            panic!("set mock socket read timeout: {error}");
                        }
                        let mut reader = BufReader::new(stream.try_clone().unwrap());
                        let mut line = String::new();
                        if reader.read_line(&mut line).unwrap_or(0) == 0 {
                            continue;
                        }
                        let Ok(envelope) = serde_json::from_str::<Envelope>(line.trim()) else {
                            continue;
                        };
                        let request = envelope.request.clone();
                        let request_id = envelope.request_id.clone();
                        let reply = handler(&request);
                        reqs.lock().unwrap().push(request);
                        envs.lock().unwrap().push(envelope);
                        if let Some(resp) = reply {
                            let mut out =
                                serde_json::to_string(&ResponseEnvelope::new(request_id, resp))
                                    .unwrap();
                            out.push('\n');
                            let mut stream = stream;
                            let _ = stream.write_all(out.as_bytes());
                        }
                    }
                    Err(_) => std::thread::sleep(Duration::from_millis(10)),
                }
            }
        });

        Self {
            requests,
            envelopes,
            stop,
            handle: Some(handle),
        }
    }

    pub fn request_count(&self) -> usize {
        self.requests.lock().unwrap().len()
    }
}

impl Drop for MockDaemon {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}
