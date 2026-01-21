//! OxidePM Web API Server
//!
//! Provides REST API and WebSocket support for remote process management.

use axum::{
    extract::{Path, Request as AxumRequest, State, WebSocketUpgrade},
    http::{header::HeaderValue, Method, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Json, Response as AxumResponse},
    routing::{delete, get, post},
    Router,
};
use futures::{SinkExt, StreamExt};
use oxidepm_core::{AppInfo, AppSpec, Selector};
use oxidepm_ipc::{IpcClient, Request, Response};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::broadcast;
use tower_http::cors::CorsLayer;
use tower_http::trace::TraceLayer;
use tracing::info;

/// API response wrapper
#[derive(Serialize)]
pub struct ApiResponse<T> {
    pub success: bool,
    pub data: Option<T>,
    pub error: Option<String>,
}

impl<T: Serialize> ApiResponse<T> {
    pub fn ok(data: T) -> Self {
        Self {
            success: true,
            data: Some(data),
            error: None,
        }
    }

    pub fn err(message: impl Into<String>) -> ApiResponse<()> {
        ApiResponse {
            success: false,
            data: None,
            error: Some(message.into()),
        }
    }
}

/// Shared application state
#[derive(Clone)]
pub struct AppState {
    client: Arc<IpcClient>,
    event_tx: broadcast::Sender<WebEvent>,
    #[allow(dead_code)]
    api_key: Option<String>,
}

impl AppState {
    pub fn new(socket_path: std::path::PathBuf, api_key: Option<String>) -> Self {
        let (event_tx, _) = broadcast::channel(100);
        Self {
            client: Arc::new(IpcClient::new(socket_path)),
            event_tx,
            api_key,
        }
    }
}

/// API key authentication middleware
async fn api_key_auth(
    State(state): State<AppState>,
    request: AxumRequest,
    next: Next,
) -> Result<AxumResponse, StatusCode> {
    // If no API key is configured, allow all requests
    let Some(expected_key) = &state.api_key else {
        return Ok(next.run(request).await);
    };

    // Check the X-API-Key header
    let provided_key = request
        .headers()
        .get("X-API-Key")
        .and_then(|v| v.to_str().ok());

    match provided_key {
        Some(key) if key == expected_key => Ok(next.run(request).await),
        _ => Err(StatusCode::UNAUTHORIZED),
    }
}

/// WebSocket event types
#[derive(Clone, Serialize)]
#[serde(tag = "type")]
pub enum WebEvent {
    ProcessStarted { id: u32, name: String },
    ProcessStopped { id: u32, name: String },
    ProcessRestarted { id: u32, name: String },
    MetricsUpdate { processes: Vec<ProcessMetrics> },
    LogLine { app_id: u32, line: String },
}

#[derive(Clone, Serialize)]
pub struct ProcessMetrics {
    pub id: u32,
    pub name: String,
    pub cpu: f32,
    pub memory: u64,
    pub uptime: u64,
    pub status: String,
}

/// Start request body
#[derive(Deserialize)]
pub struct StartRequest {
    pub name: String,
    pub command: String,
    pub cwd: Option<String>,
    pub args: Option<Vec<String>>,
    pub env: Option<std::collections::HashMap<String, String>>,
    pub instances: Option<u32>,
    pub watch: Option<bool>,
    pub port: Option<u16>,
}

/// Create the API router
pub fn create_router(state: AppState) -> Router {
    create_router_with_cors(state, None)
}

/// Create the API router with custom CORS origin
pub fn create_router_with_cors(state: AppState, cors_origin: Option<String>) -> Router {
    // Default to localhost only for security; allow custom origin via flag
    let cors = match cors_origin {
        Some(origin) => CorsLayer::new()
            .allow_origin(origin.parse::<HeaderValue>().unwrap_or_else(|_| {
                "http://localhost:3000".parse().unwrap()
            }))
            .allow_methods([Method::GET, Method::POST, Method::DELETE, Method::OPTIONS])
            .allow_headers(tower_http::cors::Any),
        None => CorsLayer::new()
            .allow_origin("http://localhost:3000".parse::<HeaderValue>().unwrap())
            .allow_methods([Method::GET, Method::POST, Method::DELETE, Method::OPTIONS])
            .allow_headers(tower_http::cors::Any),
    };

    // Routes that require authentication
    let protected_routes = Router::new()
        // Process management
        .route("/api/processes", get(list_processes))
        .route("/api/processes", post(start_process))
        .route("/api/processes/:selector", get(get_process))
        .route("/api/processes/:selector", delete(delete_process))
        .route("/api/processes/:selector/stop", post(stop_process))
        .route("/api/processes/:selector/restart", post(restart_process))
        .route("/api/processes/:selector/logs", get(get_logs))
        // System (except health)
        .route("/api/ping", get(ping_daemon))
        .route("/api/save", post(save_processes))
        .route("/api/resurrect", post(resurrect_processes))
        // WebSocket for real-time updates
        .route("/ws", get(websocket_handler))
        .route_layer(middleware::from_fn_with_state(state.clone(), api_key_auth));

    // Public routes (no auth required)
    let public_routes = Router::new()
        .route("/api/health", get(health_check));

    Router::new()
        .merge(public_routes)
        .merge(protected_routes)
        .layer(cors)
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

/// Start the web server
pub async fn start_server(
    bind_addr: &str,
    socket_path: std::path::PathBuf,
    api_key: Option<String>,
) -> std::io::Result<()> {
    start_server_with_cors(bind_addr, socket_path, api_key, None).await
}

/// Start the web server with custom CORS origin
pub async fn start_server_with_cors(
    bind_addr: &str,
    socket_path: std::path::PathBuf,
    api_key: Option<String>,
    cors_origin: Option<String>,
) -> std::io::Result<()> {
    let state = AppState::new(socket_path, api_key);
    let app = create_router_with_cors(state.clone(), cors_origin);

    info!("Starting OxidePM Web API on {}", bind_addr);

    let listener = tokio::net::TcpListener::bind(bind_addr).await?;
    axum::serve(listener, app).await?;

    Ok(())
}

// === API Handlers ===

async fn health_check() -> impl IntoResponse {
    Json(ApiResponse::ok(serde_json::json!({
        "status": "healthy",
        "version": env!("CARGO_PKG_VERSION")
    })))
}

async fn ping_daemon(State(state): State<AppState>) -> impl IntoResponse {
    match state.client.send(&Request::Ping).await {
        Ok(Response::Pong) => Json(ApiResponse::ok("pong")).into_response(),
        Ok(Response::Error { message }) => {
            (StatusCode::SERVICE_UNAVAILABLE, Json(ApiResponse::<()>::err(message))).into_response()
        }
        Err(e) => {
            (StatusCode::SERVICE_UNAVAILABLE, Json(ApiResponse::<()>::err(e.to_string()))).into_response()
        }
        _ => (StatusCode::INTERNAL_SERVER_ERROR, Json(ApiResponse::<()>::err("Unexpected response"))).into_response()
    }
}

async fn list_processes(State(state): State<AppState>) -> impl IntoResponse {
    match state.client.send(&Request::Status).await {
        Ok(Response::Status { apps }) => Json(ApiResponse::ok(apps)).into_response(),
        Ok(Response::Error { message }) => {
            (StatusCode::INTERNAL_SERVER_ERROR, Json(ApiResponse::<Vec<AppInfo>>::err(message))).into_response()
        }
        Err(e) => {
            (StatusCode::INTERNAL_SERVER_ERROR, Json(ApiResponse::<Vec<AppInfo>>::err(e.to_string()))).into_response()
        }
        _ => (StatusCode::INTERNAL_SERVER_ERROR, Json(ApiResponse::<Vec<AppInfo>>::err("Unexpected response"))).into_response()
    }
}

async fn get_process(
    State(state): State<AppState>,
    Path(selector): Path<String>,
) -> impl IntoResponse {
    let selector = Selector::parse(&selector);
    match state.client.send(&Request::Show { selector }).await {
        Ok(Response::Show { app }) => Json(ApiResponse::ok(app)).into_response(),
        Ok(Response::Error { message }) => {
            (StatusCode::NOT_FOUND, Json(ApiResponse::<AppInfo>::err(message))).into_response()
        }
        Err(e) => {
            (StatusCode::INTERNAL_SERVER_ERROR, Json(ApiResponse::<AppInfo>::err(e.to_string()))).into_response()
        }
        _ => (StatusCode::INTERNAL_SERVER_ERROR, Json(ApiResponse::<AppInfo>::err("Unexpected response"))).into_response()
    }
}

async fn start_process(
    State(state): State<AppState>,
    Json(req): Json<StartRequest>,
) -> impl IntoResponse {
    use oxidepm_core::AppMode;
    use std::path::PathBuf;

    let cwd = req.cwd.map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."));
    let mode = AppMode::detect(&cwd).unwrap_or(AppMode::Cmd);

    let mut spec = AppSpec::new(req.name, mode, req.command, cwd);

    if let Some(args) = req.args {
        spec.args = args;
    }
    if let Some(env) = req.env {
        spec.env = env;
    }
    if let Some(instances) = req.instances {
        spec.instances = instances;
    }
    if let Some(watch) = req.watch {
        spec.watch = watch;
    }
    if let Some(port) = req.port {
        spec.port = Some(port);
    }

    match state.client.send(&Request::Start { spec }).await {
        Ok(Response::Started { id, name }) => {
            let _ = state.event_tx.send(WebEvent::ProcessStarted { id, name: name.clone() });
            Json(ApiResponse::ok(serde_json::json!({ "id": id, "name": name }))).into_response()
        }
        Ok(Response::Error { message }) => {
            (StatusCode::BAD_REQUEST, Json(ApiResponse::<()>::err(message))).into_response()
        }
        Err(e) => {
            (StatusCode::INTERNAL_SERVER_ERROR, Json(ApiResponse::<()>::err(e.to_string()))).into_response()
        }
        _ => (StatusCode::INTERNAL_SERVER_ERROR, Json(ApiResponse::<()>::err("Unexpected response"))).into_response()
    }
}

async fn stop_process(
    State(state): State<AppState>,
    Path(selector): Path<String>,
) -> impl IntoResponse {
    let selector = Selector::parse(&selector);
    match state.client.send(&Request::Stop { selector }).await {
        Ok(Response::Stopped { count }) => Json(ApiResponse::ok(serde_json::json!({ "stopped": count }))).into_response(),
        Ok(Response::Error { message }) => {
            (StatusCode::BAD_REQUEST, Json(ApiResponse::<()>::err(message))).into_response()
        }
        Err(e) => {
            (StatusCode::INTERNAL_SERVER_ERROR, Json(ApiResponse::<()>::err(e.to_string()))).into_response()
        }
        _ => (StatusCode::INTERNAL_SERVER_ERROR, Json(ApiResponse::<()>::err("Unexpected response"))).into_response()
    }
}

async fn restart_process(
    State(state): State<AppState>,
    Path(selector): Path<String>,
) -> impl IntoResponse {
    let selector = Selector::parse(&selector);
    match state.client.send(&Request::Restart { selector }).await {
        Ok(Response::Restarted { count }) => Json(ApiResponse::ok(serde_json::json!({ "restarted": count }))).into_response(),
        Ok(Response::Error { message }) => {
            (StatusCode::BAD_REQUEST, Json(ApiResponse::<()>::err(message))).into_response()
        }
        Err(e) => {
            (StatusCode::INTERNAL_SERVER_ERROR, Json(ApiResponse::<()>::err(e.to_string()))).into_response()
        }
        _ => (StatusCode::INTERNAL_SERVER_ERROR, Json(ApiResponse::<()>::err("Unexpected response"))).into_response()
    }
}

async fn delete_process(
    State(state): State<AppState>,
    Path(selector): Path<String>,
) -> impl IntoResponse {
    let selector = Selector::parse(&selector);
    match state.client.send(&Request::Delete { selector }).await {
        Ok(Response::Deleted { count }) => Json(ApiResponse::ok(serde_json::json!({ "deleted": count }))).into_response(),
        Ok(Response::Error { message }) => {
            (StatusCode::BAD_REQUEST, Json(ApiResponse::<()>::err(message))).into_response()
        }
        Err(e) => {
            (StatusCode::INTERNAL_SERVER_ERROR, Json(ApiResponse::<()>::err(e.to_string()))).into_response()
        }
        _ => (StatusCode::INTERNAL_SERVER_ERROR, Json(ApiResponse::<()>::err("Unexpected response"))).into_response()
    }
}

async fn get_logs(
    State(state): State<AppState>,
    Path(selector): Path<String>,
) -> impl IntoResponse {
    let selector = Selector::parse(&selector);
    match state.client.send(&Request::Logs { selector, lines: 100, follow: false, stdout: true, stderr: true }).await {
        Ok(Response::LogLines { lines }) => Json(ApiResponse::ok(lines)).into_response(),
        Ok(Response::Error { message }) => {
            (StatusCode::BAD_REQUEST, Json(ApiResponse::<Vec<String>>::err(message))).into_response()
        }
        Err(e) => {
            (StatusCode::INTERNAL_SERVER_ERROR, Json(ApiResponse::<Vec<String>>::err(e.to_string()))).into_response()
        }
        _ => (StatusCode::INTERNAL_SERVER_ERROR, Json(ApiResponse::<Vec<String>>::err("Unexpected response"))).into_response()
    }
}

async fn save_processes(State(state): State<AppState>) -> impl IntoResponse {
    match state.client.send(&Request::Save).await {
        Ok(Response::Saved { count, path }) => {
            Json(ApiResponse::ok(serde_json::json!({ "saved": count, "path": path }))).into_response()
        }
        Ok(Response::Error { message }) => {
            (StatusCode::INTERNAL_SERVER_ERROR, Json(ApiResponse::<()>::err(message))).into_response()
        }
        Err(e) => {
            (StatusCode::INTERNAL_SERVER_ERROR, Json(ApiResponse::<()>::err(e.to_string()))).into_response()
        }
        _ => (StatusCode::INTERNAL_SERVER_ERROR, Json(ApiResponse::<()>::err("Unexpected response"))).into_response()
    }
}

async fn resurrect_processes(State(state): State<AppState>) -> impl IntoResponse {
    match state.client.send(&Request::Resurrect).await {
        Ok(Response::Resurrected { count }) => Json(ApiResponse::ok(serde_json::json!({ "resurrected": count }))).into_response(),
        Ok(Response::Error { message }) => {
            (StatusCode::INTERNAL_SERVER_ERROR, Json(ApiResponse::<()>::err(message))).into_response()
        }
        Err(e) => {
            (StatusCode::INTERNAL_SERVER_ERROR, Json(ApiResponse::<()>::err(e.to_string()))).into_response()
        }
        _ => (StatusCode::INTERNAL_SERVER_ERROR, Json(ApiResponse::<()>::err("Unexpected response"))).into_response()
    }
}

async fn websocket_handler(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_websocket(socket, state))
}

async fn handle_websocket(
    socket: axum::extract::ws::WebSocket,
    state: AppState,
) {
    use axum::extract::ws::Message;

    let (mut sender, mut receiver) = socket.split();
    let mut event_rx = state.event_tx.subscribe();

    // Spawn task to send events to client
    let send_task = tokio::spawn(async move {
        while let Ok(event) = event_rx.recv().await {
            if let Ok(json) = serde_json::to_string(&event) {
                if sender.send(Message::Text(json)).await.is_err() {
                    break;
                }
            }
        }
    });

    // Handle incoming messages (for commands via WebSocket)
    while let Some(Ok(msg)) = receiver.next().await {
        match msg {
            Message::Text(text) => {
                info!("WebSocket received: {}", text);
                // Could handle commands here
            }
            Message::Close(_) => break,
            _ => {}
        }
    }

    send_task.abort();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_api_response_ok() {
        let resp = ApiResponse::ok("test");
        assert!(resp.success);
        assert_eq!(resp.data, Some("test"));
        assert!(resp.error.is_none());
    }

    #[test]
    fn test_api_response_err() {
        let resp = ApiResponse::<()>::err("error message");
        assert!(!resp.success);
        assert!(resp.data.is_none());
        assert_eq!(resp.error, Some("error message".to_string()));
    }
}
