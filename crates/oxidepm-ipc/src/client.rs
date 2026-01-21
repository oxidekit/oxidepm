//! IPC Client - Unix socket client for CLI

use oxidepm_core::{Error, Result};
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tracing::{debug, info, warn};

use crate::protocol::{Request, Response};

/// IPC Client for CLI communication with daemon
pub struct IpcClient {
    socket_path: PathBuf,
}

impl IpcClient {
    /// Create a new IPC client
    pub fn new(socket_path: PathBuf) -> Self {
        Self { socket_path }
    }

    /// Check if daemon is running
    pub fn is_daemon_running(&self) -> bool {
        self.socket_path.exists()
    }

    /// Connect to daemon (without auto-start)
    pub async fn connect(&self) -> Result<UnixStream> {
        if !self.socket_path.exists() {
            return Err(Error::DaemonNotRunning);
        }

        UnixStream::connect(&self.socket_path)
            .await
            .map_err(|e| match e.kind() {
                std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused => {
                    Error::DaemonNotRunning
                }
                _ => Error::IpcConnectionFailed(e.to_string()),
            })
    }

    /// Connect to daemon, starting it if necessary (PM2 behavior)
    pub async fn connect_or_start(&self) -> Result<UnixStream> {
        match self.connect().await {
            Ok(stream) => Ok(stream),
            Err(Error::DaemonNotRunning) => {
                info!("Daemon not running, starting...");
                self.start_daemon()?;

                // Wait for daemon to be ready (up to 5 seconds)
                for i in 0..50 {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    if let Ok(stream) = self.connect().await {
                        info!("Connected to daemon after {}ms", (i + 1) * 100);
                        return Ok(stream);
                    }
                }

                Err(Error::IpcError(
                    "Daemon failed to start within timeout".to_string(),
                ))
            }
            Err(e) => Err(e),
        }
    }

    /// Start the daemon process
    fn start_daemon(&self) -> Result<()> {
        // Find oxidepmd binary in same directory as oxidepm
        let exe = std::env::current_exe()?;
        let exe_dir = exe.parent().ok_or_else(|| {
            Error::IpcError("Cannot determine executable directory".to_string())
        })?;

        let daemon_path = exe_dir.join("oxidepmd");
        if !daemon_path.exists() {
            // Try in PATH
            warn!("oxidepmd not found at {}, trying PATH", daemon_path.display());
        }

        let daemon_exe = if daemon_path.exists() {
            daemon_path
        } else {
            PathBuf::from("oxidepmd")
        };

        info!("Starting daemon: {}", daemon_exe.display());

        Command::new(&daemon_exe)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .map_err(|e| Error::IpcError(format!("Failed to start daemon: {}", e)))?;

        Ok(())
    }

    /// Send a request and receive response
    pub async fn send(&self, request: &Request) -> Result<Response> {
        let mut stream = self.connect_or_start().await?;

        // Send request
        let mut json = serde_json::to_string(request)?;
        json.push('\n');

        stream
            .write_all(json.as_bytes())
            .await
            .map_err(|e| Error::IpcError(format!("Write error: {}", e)))?;

        stream
            .flush()
            .await
            .map_err(|e| Error::IpcError(format!("Flush error: {}", e)))?;

        debug!("Sent request: {:?}", request);

        // Read response
        let mut reader = BufReader::new(stream);
        let mut line = String::new();

        reader
            .read_line(&mut line)
            .await
            .map_err(|e| Error::IpcError(format!("Read error: {}", e)))?;

        let response: Response = serde_json::from_str(line.trim())
            .map_err(|e| Error::IpcError(format!("Invalid response: {}", e)))?;

        debug!("Received response: {:?}", response);
        Ok(response)
    }

    /// Send a request and receive a stream of responses (for logs -f)
    pub async fn send_streaming<F>(&self, request: &Request, mut on_response: F) -> Result<()>
    where
        F: FnMut(Response) -> bool, // Return false to stop
    {
        let mut stream = self.connect_or_start().await?;

        // Send request
        let mut json = serde_json::to_string(request)?;
        json.push('\n');

        stream
            .write_all(json.as_bytes())
            .await
            .map_err(|e| Error::IpcError(format!("Write error: {}", e)))?;

        stream
            .flush()
            .await
            .map_err(|e| Error::IpcError(format!("Flush error: {}", e)))?;

        // Read responses until closed or callback returns false
        let mut reader = BufReader::new(stream);
        loop {
            let mut line = String::new();
            match reader.read_line(&mut line).await {
                Ok(0) => break, // Connection closed
                Ok(_) => {
                    if let Ok(response) = serde_json::from_str::<Response>(line.trim()) {
                        if !on_response(response) {
                            break;
                        }
                    }
                }
                Err(_) => break,
            }
        }

        Ok(())
    }

    /// Ping the daemon
    pub async fn ping(&self) -> Result<bool> {
        match self.send(&Request::Ping).await {
            Ok(Response::Pong) => Ok(true),
            Ok(_) => Ok(false),
            Err(Error::DaemonNotRunning) => Ok(false),
            Err(e) => Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_client_creation() {
        let client = IpcClient::new(PathBuf::from("/tmp/test.sock"));
        assert_eq!(client.socket_path, PathBuf::from("/tmp/test.sock"));
    }

    #[tokio::test]
    async fn test_connect_no_daemon() {
        let dir = tempdir().unwrap();
        let socket_path = dir.path().join("nonexistent.sock");

        let client = IpcClient::new(socket_path);
        let result = client.connect().await;

        assert!(matches!(result, Err(Error::DaemonNotRunning)));
    }
}
