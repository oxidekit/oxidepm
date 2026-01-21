//! Main daemon orchestration

use oxidepm_core::{constants, Result};
use oxidepm_db::Database;
use oxidepm_ipc::{IpcServer, Request, Response};
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{error, info};

use crate::handlers::RequestHandler;
use crate::supervisor::Supervisor;

/// Main daemon struct
pub struct Daemon {
    server: IpcServer,
    handler: Arc<RwLock<RequestHandler>>,
}

impl Daemon {
    /// Create a new daemon instance
    pub async fn new() -> Result<Self> {
        // Initialize database
        let db_path = constants::db_path();
        let db = Database::new(&db_path).await?;
        info!("Database initialized at {}", db_path.display());

        // Create supervisor
        let supervisor = Supervisor::new(db).await?;

        // Resurrect any saved processes
        let count = supervisor.resurrect().await?;
        if count > 0 {
            info!("Resurrected {} saved processes", count);
        }

        // Create request handler
        let handler = RequestHandler::new(supervisor);

        // Create IPC server
        let socket_path = constants::socket_path();
        let server = IpcServer::bind(&socket_path).await?;
        info!("IPC server listening on {}", socket_path.display());

        Ok(Self {
            server,
            handler: Arc::new(RwLock::new(handler)),
        })
    }

    /// Run the daemon main loop
    pub async fn run(&self) -> Result<()> {
        info!("Daemon running, waiting for connections...");

        loop {
            match self.server.accept().await {
                Ok(mut conn) => {
                    let handler = Arc::clone(&self.handler);

                    tokio::spawn(async move {
                        loop {
                            match conn.read_request().await {
                                Ok(Some(request)) => {
                                    let response = Self::handle_request(&handler, request).await;

                                    if let Err(e) = conn.send_response(&response).await {
                                        error!("Failed to send response: {}", e);
                                        break;
                                    }

                                    // Check if this was a kill request
                                    if matches!(response, Response::Ok { .. })
                                        && matches!(response, Response::Ok { message } if message.contains("Daemon shutting down"))
                                    {
                                        // Don't break here, let the main loop handle shutdown
                                    }
                                }
                                Ok(None) => {
                                    // Connection closed
                                    break;
                                }
                                Err(e) => {
                                    error!("Error reading request: {}", e);
                                    break;
                                }
                            }
                        }
                    });
                }
                Err(e) => {
                    error!("Failed to accept connection: {}", e);
                }
            }
        }
    }

    async fn handle_request(
        handler: &Arc<RwLock<RequestHandler>>,
        request: Request,
    ) -> Response {
        let mut h = handler.write().await;

        match request {
            Request::Ping => Response::Pong,
            Request::Start { spec } => h.start(spec).await,
            Request::Stop { selector } => h.stop(selector).await,
            Request::Restart { selector } => h.restart(selector).await,
            Request::Delete { selector } => h.delete(selector).await,
            Request::Status => h.status().await,
            Request::Show { selector } => h.show(selector).await,
            Request::Logs {
                selector,
                lines,
                follow: _,
                stdout,
                stderr,
            } => h.logs(selector, lines, stdout, stderr).await,
            Request::Save => h.save().await,
            Request::Resurrect => h.resurrect().await,
            Request::Reload { selector } => h.reload(selector).await,
            Request::Flush { selector } => h.flush(selector).await,
            Request::Describe { selector } => h.describe(selector).await,
            Request::Kill => {
                // Save before killing
                let _ = h.save().await;
                Response::ok("Daemon shutting down")
            }
        }
    }
}
