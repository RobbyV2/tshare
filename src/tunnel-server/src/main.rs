use anyhow::Result;
use axum::extract::ws::{Message, WebSocket};
use axum::extract::{Path, State, WebSocketUpgrade};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use clap::Parser;
use dashmap::DashMap;
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::{RwLock, broadcast, mpsc};
use tracing::{error, info, warn};
use uuid::Uuid;

type SessionMap = Arc<DashMap<String, SessionState>>;

#[derive(Clone)]
struct SessionState {
    session_id: String,
    owner_password_hash: Option<String>,
    guest_password_hash: Option<String>,
    is_guest_readonly: bool,
    // Broadcast channel for PTY data going to web clients
    pty_broadcast: broadcast::Sender<Vec<u8>>,
    // Sender for web data going to PTY client
    web_to_pty_tx: Arc<RwLock<Option<mpsc::Sender<Vec<u8>>>>>,
}

#[derive(Clone)]
struct AppState {
    sessions: SessionMap,
}

#[derive(Deserialize)]
struct CreateSessionRequest {
    owner_password: Option<String>,
    guest_password: Option<String>,
    is_guest_readonly: bool,
}

#[derive(Serialize)]
struct CreateSessionResponse {
    session_id: String,
}

#[derive(Serialize)]
struct SessionDetails {
    session_id: String,
    owner_password_hash: Option<String>,
    guest_password_hash: Option<String>,
    is_guest_readonly: bool,
}

#[derive(Parser, Debug)]
#[command(
    author,
    version,
    about = "TShare tunnel server - handles PTY data streams"
)]
struct Args {
    /// Host to bind the API server to
    #[arg(long, default_value = "127.0.0.1")]
    api_host: String,

    /// Port for the API server
    #[arg(long, default_value_t = 8081)]
    api_port: u16,

    /// Host to bind the WebSocket server to
    #[arg(long, default_value = "127.0.0.1")]
    ws_host: String,

    /// Port for the WebSocket server
    #[arg(long, default_value_t = 8080)]
    ws_port: u16,
}

#[tokio::main]
async fn main() -> Result<()> {
    // Create ~/.tshare directory if it doesn't exist
    let home_dir = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    let tshare_dir = format!("{home_dir}/.tshare");
    std::fs::create_dir_all(&tshare_dir)?;

    // Clear previous log file
    let log_path = format!("{tshare_dir}/tunnel-server.log");
    let _ = std::fs::remove_file(&log_path);

    // Configure logging to both console and file
    let log_file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&log_path)?;

    use tracing_subscriber::fmt::writer::MakeWriterExt;
    let writer = std::io::stdout.and(log_file);

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_writer(writer)
        .init();

    let args = Args::parse();

    let sessions: SessionMap = Arc::new(DashMap::new());
    let app_state = AppState { sessions };

    // API server
    let api_app = Router::new()
        .route("/api/session", post(create_session))
        .route("/api/session/{id}", get(get_session))
        .with_state(app_state.clone());

    // WebSocket server
    let ws_app = Router::new()
        .route("/ws/pty/{id}", get(handle_pty_ws))
        .route("/ws/web/{id}", get(handle_web_ws))
        .with_state(app_state);

    let api_addr = format!("{}:{}", args.api_host, args.api_port);
    let ws_addr = format!("{}:{}", args.ws_host, args.ws_port);

    let api_listener = tokio::net::TcpListener::bind(&api_addr).await?;
    let ws_listener = tokio::net::TcpListener::bind(&ws_addr).await?;

    info!("Tunnel server starting");
    info!("API server listening on {}", api_addr);
    info!("WebSocket server listening on {}", ws_addr);

    tokio::select! {
        result = axum::serve(api_listener, api_app) => {
            error!("API server error: {:?}", result);
        }
        result = axum::serve(ws_listener, ws_app) => {
            error!("WebSocket server error: {:?}", result);
        }
    }

    Ok(())
}

async fn create_session(
    State(state): State<AppState>,
    Json(request): Json<CreateSessionRequest>,
) -> Result<Json<CreateSessionResponse>, StatusCode> {
    let session_id = Uuid::new_v4().to_string();

    info!("Creating new session: {}", session_id);
    info!(
        "Session config - readonly: {}, has_owner_pass: {}, has_guest_pass: {}",
        request.is_guest_readonly,
        request.owner_password.is_some(),
        request.guest_password.is_some()
    );

    let owner_password_hash = match request.owner_password {
        Some(password) => {
            let hash = bcrypt::hash(password, bcrypt::DEFAULT_COST).map_err(|e| {
                error!(
                    "Failed to hash owner password for session {}: {}",
                    session_id, e
                );
                StatusCode::INTERNAL_SERVER_ERROR
            })?;
            Some(hash)
        }
        None => None,
    };

    let guest_password_hash = match request.guest_password {
        Some(password) => {
            let hash = bcrypt::hash(password, bcrypt::DEFAULT_COST).map_err(|e| {
                error!(
                    "Failed to hash guest password for session {}: {}",
                    session_id, e
                );
                StatusCode::INTERNAL_SERVER_ERROR
            })?;
            Some(hash)
        }
        None => None,
    };

    let (pty_broadcast, _) = broadcast::channel(1024);
    let web_to_pty_tx = Arc::new(RwLock::new(None));

    let session = SessionState {
        session_id: session_id.clone(),
        owner_password_hash,
        guest_password_hash,
        is_guest_readonly: request.is_guest_readonly,
        pty_broadcast,
        web_to_pty_tx,
    };

    state.sessions.insert(session_id.clone(), session);

    info!(
        "Successfully created session: {} (total sessions: {})",
        session_id,
        state.sessions.len()
    );

    Ok(Json(CreateSessionResponse { session_id }))
}

async fn get_session(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
) -> Result<Json<SessionDetails>, StatusCode> {
    info!("Looking up session: {}", session_id);

    let session = state.sessions.get(&session_id).ok_or_else(|| {
        warn!("Session not found: {}", session_id);
        StatusCode::NOT_FOUND
    })?;

    let details = SessionDetails {
        session_id: session.session_id.clone(),
        owner_password_hash: session.owner_password_hash.clone(),
        guest_password_hash: session.guest_password_hash.clone(),
        is_guest_readonly: session.is_guest_readonly,
    };

    info!("Retrieved session details for: {}", session_id);
    Ok(Json(details))
}

async fn handle_pty_ws(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    ws: WebSocketUpgrade,
) -> Response {
    let session = match state.sessions.get(&session_id) {
        Some(session_ref) => session_ref.clone(),
        None => return StatusCode::NOT_FOUND.into_response(),
    };

    ws.on_upgrade(move |socket| handle_pty_websocket(socket, session))
}

async fn handle_web_ws(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    ws: WebSocketUpgrade,
) -> Response {
    let session = match state.sessions.get(&session_id) {
        Some(session_ref) => session_ref.clone(),
        None => return StatusCode::NOT_FOUND.into_response(),
    };

    ws.on_upgrade(move |socket| handle_web_websocket(socket, session))
}

async fn handle_pty_websocket(socket: WebSocket, session: SessionState) {
    let (mut ws_sender, mut ws_receiver) = socket.split();
    let session_id = session.session_id.clone();

    info!("PTY WebSocket connected for session: {}", session_id);

    // Create channel for receiving web-to-pty data
    let (web_to_pty_tx, mut web_to_pty_rx) = mpsc::channel(1024);

    // Store the sender in the session
    {
        let mut tx_guard = session.web_to_pty_tx.write().await;
        *tx_guard = Some(web_to_pty_tx);
        info!("Stored web-to-pty channel for session: {}", session_id);
    }

    // Task to forward PTY data to web clients via broadcast
    let pty_broadcast = session.pty_broadcast.clone();
    let session_id_clone = session_id.clone();
    let forward_pty_to_web = tokio::spawn(async move {
        info!(
            "Starting PTY-to-web forwarding for session: {}",
            session_id_clone
        );
        let mut session_ended_sent = false;
        while let Some(msg) = ws_receiver.next().await {
            match msg {
                Ok(Message::Binary(data)) => {
                    let subscribers = pty_broadcast.receiver_count();
                    let result = pty_broadcast.send(data.to_vec());
                    if result.is_ok() && subscribers > 0 {
                        info!(
                            "Forwarded {} bytes to {} web clients for session: {}",
                            data.len(),
                            subscribers,
                            session_id_clone
                        );
                    }
                }
                Ok(Message::Close(_)) => {
                    info!(
                        "PTY WebSocket close message received for session: {}",
                        session_id_clone
                    );
                    if !session_ended_sent {
                        let _ = pty_broadcast.send(b"__TSHARE_SESSION_ENDED__".to_vec());
                        session_ended_sent = true;
                    }
                    break;
                }
                Err(e) => {
                    warn!(
                        "PTY WebSocket error for session {}: {}",
                        session_id_clone, e
                    );
                    if !session_ended_sent {
                        let _ = pty_broadcast.send(b"__TSHARE_SESSION_ENDED__".to_vec());
                        session_ended_sent = true;
                    }
                    break;
                }
                _ => {}
            }
        }
        info!(
            "PTY-to-web forwarding ended for session: {}",
            session_id_clone
        );
        // Send session end notification only if not already sent
        if !session_ended_sent {
            let _ = pty_broadcast.send(b"__TSHARE_SESSION_ENDED__".to_vec());
        }
    });

    // Task to forward web data to PTY
    let session_id_clone = session_id.clone();
    let forward_web_to_pty = tokio::spawn(async move {
        info!(
            "Starting web-to-PTY forwarding for session: {}",
            session_id_clone
        );
        while let Some(data) = web_to_pty_rx.recv().await {
            info!(
                "Forwarding {} bytes from web to PTY for session: {}",
                data.len(),
                session_id_clone
            );
            if ws_sender.send(Message::Binary(data.into())).await.is_err() {
                warn!(
                    "Failed to send data to PTY for session: {}",
                    session_id_clone
                );
                break;
            }
        }
        info!(
            "Web-to-PTY forwarding ended for session: {}",
            session_id_clone
        );
    });

    tokio::select! {
        _ = forward_pty_to_web => {},
        _ = forward_web_to_pty => {},
    }

    // Clear the sender when PTY disconnects
    {
        let mut tx_guard = session.web_to_pty_tx.write().await;
        *tx_guard = None;
        info!("Cleared web-to-pty channel for session: {}", session_id);
    }

    info!("PTY WebSocket disconnected for session: {}", session_id);
}

async fn handle_web_websocket(socket: WebSocket, session: SessionState) {
    let (mut ws_sender, mut ws_receiver) = socket.split();
    let session_id = session.session_id.clone();
    let is_readonly = session.is_guest_readonly;

    info!(
        "Web WebSocket connected for session: {} (readonly: {})",
        session_id, is_readonly
    );

    // Subscribe to PTY broadcast
    let mut pty_broadcast_rx = session.pty_broadcast.subscribe();
    let web_to_pty_tx_ref = session.web_to_pty_tx.clone();

    // Task to forward PTY data to web client
    let session_id_clone = session_id.clone();
    let forward_pty_to_web = tokio::spawn(async move {
        info!(
            "Starting PTY-to-web forwarding for web client of session: {}",
            session_id_clone
        );
        while let Ok(data) = pty_broadcast_rx.recv().await {
            info!(
                "Sending {} bytes to web client for session: {}",
                data.len(),
                session_id_clone
            );
            if ws_sender.send(Message::Binary(data.into())).await.is_err() {
                warn!(
                    "Failed to send data to web client for session: {}",
                    session_id_clone
                );
                break;
            }
        }
        info!(
            "PTY-to-web forwarding ended for web client of session: {}",
            session_id_clone
        );
    });

    // Task to forward web data to PTY (if not readonly)
    let session_id_clone = session_id.clone();
    let forward_web_to_pty = tokio::spawn(async move {
        info!(
            "Starting web-to-PTY forwarding for session: {} (readonly: {})",
            session_id_clone, is_readonly
        );
        while let Some(msg) = ws_receiver.next().await {
            match msg {
                Ok(Message::Binary(data)) => {
                    if !is_readonly {
                        info!(
                            "Received {} bytes from web client for session: {}",
                            data.len(),
                            session_id_clone
                        );
                        let tx_guard = web_to_pty_tx_ref.read().await;
                        if let Some(web_to_pty_tx) = tx_guard.as_ref() {
                            if web_to_pty_tx.send(data.to_vec()).await.is_err() {
                                warn!(
                                    "Failed to forward web data to PTY for session: {}",
                                    session_id_clone
                                );
                                break;
                            }
                        } else {
                            warn!(
                                "No PTY connection available for session: {}",
                                session_id_clone
                            );
                        }
                    } else {
                        info!(
                            "Ignoring input from web client (readonly mode) for session: {}",
                            session_id_clone
                        );
                    }
                }
                Ok(Message::Text(text)) => {
                    // Handle control messages like resize
                    if let Ok(msg) = serde_json::from_str::<serde_json::Value>(&text) {
                        if msg.get("type").and_then(|v| v.as_str()) == Some("resize") {
                            if let (Some(cols), Some(rows)) = (
                                msg.get("cols").and_then(|v| v.as_u64()),
                                msg.get("rows").and_then(|v| v.as_u64()),
                            ) {
                                info!(
                                    "Received resize request: {}x{} for session: {}",
                                    cols, rows, session_id_clone
                                );
                                // Forward resize message to PTY client
                                let tx_guard = web_to_pty_tx_ref.read().await;
                                if let Some(web_to_pty_tx) = tx_guard.as_ref() {
                                    let resize_msg = format!("RESIZE:{cols}:{rows}");
                                    if web_to_pty_tx.send(resize_msg.into_bytes()).await.is_err() {
                                        warn!(
                                            "Failed to send resize command to PTY for session: {}",
                                            session_id_clone
                                        );
                                    }
                                }
                            }
                        }
                    }
                }
                Ok(Message::Close(_)) => {
                    info!(
                        "Web WebSocket close message received for session: {}",
                        session_id_clone
                    );
                    break;
                }
                Err(e) => {
                    warn!(
                        "Web WebSocket error for session {}: {}",
                        session_id_clone, e
                    );
                    break;
                }
                _ => {}
            }
        }
        info!(
            "Web-to-PTY forwarding ended for session: {}",
            session_id_clone
        );
    });

    tokio::select! {
        _ = forward_pty_to_web => {},
        _ = forward_web_to_pty => {},
    }

    info!("Web WebSocket disconnected for session: {}", session_id);
}
