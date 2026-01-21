//! IPC Server - Unix socket server for daemon

use oxidepm_core::{Error, Result};
use std::path::{Path, PathBuf};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tracing::{debug, error, info};

/// Maximum IPC message size (10MB) to prevent memory exhaustion attacks
const MAX_MESSAGE_SIZE: u64 = 10 * 1024 * 1024;

use crate::protocol::{Request, Response};

/// IPC Server for daemon
pub struct IpcServer {
    socket_path: PathBuf,
    listener: UnixListener,
}

impl IpcServer {
    /// Bind to a Unix socket
    pub async fn bind(socket_path: &Path) -> Result<Self> {
        // Remove stale socket if exists
        if socket_path.exists() {
            std::fs::remove_file(socket_path)?;
        }

        // Ensure parent directory exists
        if let Some(parent) = socket_path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let listener = UnixListener::bind(socket_path)
            .map_err(|e| Error::IpcError(format!("Failed to bind socket: {}", e)))?;

        // Set socket permissions to owner-only (0600) for security
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(socket_path, std::fs::Permissions::from_mode(0o600))
                .map_err(|e| Error::IpcError(format!("Failed to set socket permissions: {}", e)))?;
        }

        info!("IPC server listening on {}", socket_path.display());

        Ok(Self {
            socket_path: socket_path.to_path_buf(),
            listener,
        })
    }

    /// Accept a new connection
    pub async fn accept(&self) -> Result<IpcConnection> {
        let (stream, _) = self
            .listener
            .accept()
            .await
            .map_err(|e| Error::IpcError(format!("Accept failed: {}", e)))?;

        debug!("Accepted IPC connection");
        Ok(IpcConnection::new(stream))
    }

    /// Get the socket path
    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }
}

impl Drop for IpcServer {
    fn drop(&mut self) {
        // Clean up socket file
        if self.socket_path.exists() {
            if let Err(e) = std::fs::remove_file(&self.socket_path) {
                error!("Failed to remove socket file: {}", e);
            }
        }
    }
}

/// Single IPC connection
pub struct IpcConnection {
    stream: UnixStream,
}

impl IpcConnection {
    pub fn new(stream: UnixStream) -> Self {
        Self { stream }
    }

    /// Read a request from the connection
    pub async fn read_request(&mut self) -> Result<Option<Request>> {
        // Limit read size to prevent memory exhaustion attacks
        let limited_reader = (&mut self.stream).take(MAX_MESSAGE_SIZE);
        let mut reader = BufReader::new(limited_reader);
        let mut line = String::new();

        match reader.read_line(&mut line).await {
            Ok(0) => Ok(None), // Connection closed
            Ok(_) => {
                let request: Request = serde_json::from_str(line.trim())
                    .map_err(|e| Error::IpcError(format!("Invalid request: {}", e)))?;
                debug!("Received request: {:?}", request);
                Ok(Some(request))
            }
            Err(e) => Err(Error::IpcError(format!("Read error: {}", e))),
        }
    }

    /// Send a response
    pub async fn send_response(&mut self, response: &Response) -> Result<()> {
        let mut json = serde_json::to_string(response)?;
        json.push('\n');

        self.stream
            .write_all(json.as_bytes())
            .await
            .map_err(|e| Error::IpcError(format!("Write error: {}", e)))?;

        self.stream
            .flush()
            .await
            .map_err(|e| Error::IpcError(format!("Flush error: {}", e)))?;

        debug!("Sent response: {:?}", response);
        Ok(())
    }

    /// Send a log line (for streaming)
    pub async fn send_log_line(&mut self, line: &str) -> Result<()> {
        let response = Response::LogLine {
            line: line.to_string(),
        };
        self.send_response(&response).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[tokio::test]
    async fn test_server_bind() {
        let dir = tempdir().unwrap();
        let socket_path = dir.path().join("test.sock");

        let server = IpcServer::bind(&socket_path).await.unwrap();
        assert!(socket_path.exists());

        drop(server);
        assert!(!socket_path.exists());
    }
}
