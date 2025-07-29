//! # TShare Tunnel Server Module
//!
//! The tunnel server acts as the central coordination hub for terminal sharing sessions.
//! It manages active sessions, handles authentication, routes data between terminal
//! clients and web viewers, and maintains session history for late-joining users.
//!
//! ## Architecture
//!
//! ```text
//! ┌─────────────────┐       ┌─────────────────┐       ┌─────────────────┐
//! │  Terminal       │       │  Tunnel Server  │       │  Web Clients    │
//! │  Client (PTY)   │◄─────►│                 │◄─────►│  (Browsers)     │
//! │                 │       │  - Session Mgmt │       │                 │
//! │  WebSocket      │       │  - Auth & Perms │       │  WebSocket      │
//! │  Connection     │       │  - Data Routing │       │  Connections    │
//! └─────────────────┘       │  - History      │       └─────────────────┘
//!                           └─────────────────┘
//! ```
//!
//! ## Key Components
//!
//! ### Session Management
//! - **Session Creation**: REST API endpoint for creating new sessions
//! - **Session Storage**: In-memory storage using concurrent HashMap (DashMap)  
//! - **Session Lifecycle**: Automatic cleanup when terminal clients disconnect
//!
//! ### Authentication & Authorization
//! - **Password Hashing**: bcrypt for secure password storage
//! - **Role-based Access**: Owner vs Guest permissions
//! - **Read-only Mode**: Optional restriction for guest users
//!
//! ### Data Routing  
//! - **Broadcast Channels**: Efficient distribution of terminal output to multiple web clients
//! - **Bidirectional Flow**: Terminal output to web, web input back to terminal
//! - **Protocol Handling**: WebSocket message routing and control message parsing
//!
//! ### History & State
//! - **Terminal History**: Buffered output for late-joining web clients
//! - **Connected Users**: Real-time tracking of active web viewers
//! - **Heartbeat System**: Connection health monitoring
//!
//! ## API Endpoints
//!
//! ### REST API
//! - `POST /api/session` - Create new session
//! - `GET /api/session/{id}` - Get session details  
//! - `GET /api/session/{id}/users` - List connected users
//! - `POST /api/session/{id}/heartbeat/{user_id}` - Update user heartbeat
//!
//! ### WebSocket Endpoints  
//! - `WS /ws/pty/{id}` - Terminal client connection
//! - `WS /ws/web/{id}?user_type={owner|guest}` - Web client connection
//!
//! ## Example Usage
//!
//! ```bash
//! # Start tunnel server on all interfaces
//! tshare tunnel --host 0.0.0.0 --port 8385
//!
//! # Start with custom configuration
//! tshare tunnel --host tunnel.example.com --port 9000
//! ```

use anyhow::Result;
use axum::extract::ws::{Message, WebSocket};
use axum::extract::{Path, Query, State, WebSocketUpgrade};
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

/// Concurrent session storage using DashMap for thread-safe access
type SessionMap = Arc<DashMap<String, SessionState>>;

/// Core session state containing all information needed to manage a terminal sharing session
///
/// Each session represents one active terminal (PTY) that can be viewed and potentially
/// controlled by multiple web users. The session manages authentication, permissions,
/// data routing, and connection tracking.
///
/// # Concurrency
/// The session state is designed for high concurrency with multiple web users connecting
/// and disconnecting while terminal output continues flowing. Uses broadcast channels
/// for efficient data distribution and async-safe locks where needed.
#[derive(Clone)]
struct SessionState {
    /// Unique session identifier (UUID)
    session_id: String,
    /// Bcrypt hash of owner password (if set)
    owner_password_hash: Option<String>,
    /// Bcrypt hash of guest password (if set)
    guest_password_hash: Option<String>,
    /// Whether guest users have read-only access
    is_guest_readonly: bool,

    /// Broadcast channel for distributing PTY output to multiple web clients
    ///
    /// Terminal output flows: PTY Client → Broadcast → Web Clients
    /// Uses tokio broadcast channel for efficient fan-out to multiple receivers
    pty_broadcast: broadcast::Sender<Vec<u8>>,

    /// Channel for routing web input back to the PTY client
    ///
    /// Web input flows: Web Clients → MPSC Channel → PTY Client  
    /// Wrapped in `Arc<RwLock>` since PTY client may disconnect/reconnect
    web_to_pty_tx: Arc<RwLock<Option<mpsc::Sender<Vec<u8>>>>>,

    /// Terminal output history buffer for late-joining web clients
    ///
    /// New web connections receive this history immediately after connecting
    /// to see previous terminal output. Memory usage grows with session duration.
    history: Arc<RwLock<Vec<u8>>>,

    /// Real-time tracking of connected web users
    ///
    /// Used for displaying connection counts and managing heartbeats.
    /// DashMap provides concurrent access without blocking other operations.
    connected_users: Arc<DashMap<String, ConnectedUser>>,
}

/// Information about a connected web user
///
/// Tracks user identity, connection time, and heartbeat for health monitoring.
/// Users are automatically removed when they stop sending heartbeats.
#[derive(Clone)]
struct ConnectedUser {
    /// User type: "owner" or "guest"
    user_type: String,
    /// Unique user identifier (UUID)
    user_id: String,
    /// Human-readable connection timestamp
    connected_at: String,
    /// Last heartbeat timestamp for connection health monitoring
    #[allow(dead_code)]
    last_heartbeat: std::sync::Arc<std::sync::RwLock<chrono::DateTime<chrono::Utc>>>,
}

#[derive(Serialize)]
struct ConnectedUserResponse {
    user_type: String,
    user_id: String,
    connected_at: String,
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

#[derive(Deserialize)]
struct WebSocketQuery {
    user_type: Option<String>,
}

#[derive(Parser, Debug)]
#[command(
    author,
    version,
    about = "TShare tunnel server - handles PTY data streams"
)]
pub struct Args {
    /// Host to bind the server to
    #[arg(long, default_value = "127.0.0.1")]
    pub host: String,

    /// Port for the server
    #[arg(long, default_value_t = 8385)]
    pub port: u16,
}

/// Starts the tunnel server for coordinating terminal sharing sessions
///
/// The tunnel server is the central hub that manages all terminal sharing sessions.
/// It provides both REST API endpoints for session management and WebSocket endpoints
/// for real-time data streaming between terminal clients and web viewers.
///
/// # Architecture
///
/// The server handles three types of connections:
/// 1. **Terminal Clients**: Connect via WebSocket to `/ws/pty/{session_id}`
/// 2. **Web Clients**: Connect via WebSocket to `/ws/web/{session_id}`
/// 3. **REST Clients**: Use HTTP endpoints for session management
///
/// # Arguments
///
/// * `args` - Server configuration including host and port
///
/// # Returns
///
/// * `Result<()>` - Success or error result
///
/// # Example
///
/// ```rust,no_run
/// use tshare::tunnel::{Args, run_tunnel_server};
///
/// # async fn example() -> anyhow::Result<()> {
/// let args = Args {
///     host: "0.0.0.0".to_string(),
///     port: 8385,
/// };
///
/// run_tunnel_server(args).await?;
/// # Ok(())
/// # }
/// ```
///
/// # Behavior
///
/// The server will:
/// - Bind to the specified host and port
/// - Accept incoming HTTP and WebSocket connections
/// - Route REST API calls to appropriate handlers
/// - Manage WebSocket connections for real-time data streaming
/// - Handle session lifecycle and cleanup
/// - Log connection events and errors
///
/// # Endpoints
///
/// See module documentation for complete endpoint listing.
pub async fn run_tunnel_server(args: Args) -> Result<()> {
    let sessions: SessionMap = Arc::new(DashMap::new());
    let app_state = AppState { sessions };

    // Combined server with both API and WebSocket routes
    let app = Router::new()
        .route("/api/session", post(create_session))
        .route("/api/session/{id}", get(get_session))
        .route("/api/session/{id}/users", get(get_connected_users))
        .route(
            "/api/session/{id}/heartbeat/{user_id}",
            post(update_heartbeat),
        )
        .route("/ws/pty/{id}", get(handle_pty_ws))
        .route("/ws/web/{id}", get(handle_web_ws))
        .with_state(app_state);

    let addr = format!("{}:{}", args.host, args.port);
    let listener = tokio::net::TcpListener::bind(&addr).await?;

    info!("Tunnel server starting");
    info!("Server listening on {} (API and WebSocket)", addr);

    if let Err(e) = axum::serve(listener, app).await {
        error!("Server error: {:?}", e);
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
    let history = Arc::new(RwLock::new(Vec::new()));
    let connected_users = Arc::new(DashMap::new());

    let session = SessionState {
        session_id: session_id.clone(),
        owner_password_hash,
        guest_password_hash,
        is_guest_readonly: request.is_guest_readonly,
        pty_broadcast,
        web_to_pty_tx,
        history,
        connected_users,
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

async fn get_connected_users(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
) -> Result<Json<Vec<ConnectedUserResponse>>, StatusCode> {
    info!("Getting connected users for session: {}", session_id);

    let session = state.sessions.get(&session_id).ok_or_else(|| {
        warn!("Session not found: {}", session_id);
        StatusCode::NOT_FOUND
    })?;

    // Clean up stale users first (older than 10 seconds)
    cleanup_stale_users(&session);

    let users: Vec<ConnectedUserResponse> = session
        .connected_users
        .iter()
        .map(|entry| {
            let user = entry.value();
            ConnectedUserResponse {
                user_type: user.user_type.clone(),
                user_id: user.user_id.clone(),
                connected_at: user.connected_at.clone(),
            }
        })
        .collect();

    info!(
        "Retrieved {} connected users for session: {}",
        users.len(),
        session_id
    );
    Ok(Json(users))
}

fn cleanup_stale_users(session: &SessionState) {
    let now = chrono::Utc::now();
    let timeout_duration = chrono::Duration::seconds(10);

    session.connected_users.retain(|_user_id, user| {
        let last_heartbeat = user.last_heartbeat.read().unwrap();
        let age = now.signed_duration_since(*last_heartbeat);
        age < timeout_duration
    });
}

async fn update_heartbeat(
    State(state): State<AppState>,
    Path((session_id, user_id)): Path<(String, String)>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let session = state.sessions.get(&session_id).ok_or_else(|| {
        warn!("Session not found: {}", session_id);
        StatusCode::NOT_FOUND
    })?;

    if let Some(user) = session.connected_users.get(&user_id) {
        let mut last_heartbeat = user.last_heartbeat.write().unwrap();
        *last_heartbeat = chrono::Utc::now();
        Ok(Json(serde_json::json!({"success": true})))
    } else {
        Err(StatusCode::NOT_FOUND)
    }
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
    Query(query): Query<WebSocketQuery>,
    ws: WebSocketUpgrade,
) -> Response {
    let session = match state.sessions.get(&session_id) {
        Some(session_ref) => session_ref.clone(),
        None => return StatusCode::NOT_FOUND.into_response(),
    };

    let user_type = query.user_type.unwrap_or_else(|| "guest".to_string());
    ws.on_upgrade(move |socket| handle_web_websocket(socket, session, user_type))
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
    let history = session.history.clone();
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
                    // Store in history
                    {
                        let mut history_guard = history.write().await;
                        history_guard.extend_from_slice(&data);
                    }

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

async fn handle_web_websocket(socket: WebSocket, session: SessionState, user_type: String) {
    let (mut ws_sender, mut ws_receiver) = socket.split();
    let session_id = session.session_id.clone();

    // Determine readonly status based on user type and session settings
    // Authentication logic:
    // - If no passwords set: auto-authenticate as owner (full access)
    // - If owner_pass provided and matches: authenticate as owner (full access)
    // - If guest_pass provided and matches: authenticate as guest (follows is_guest_readonly setting)
    // - Owners always have read/write access regardless of is_guest_readonly
    // - Guests have read/write access only if is_guest_readonly is false
    let is_readonly = match user_type.as_str() {
        "owner" => false,                     // Owners always have write access
        "guest" => session.is_guest_readonly, // Guests follow session readonly setting
        _ => true,                            // Default to readonly for unknown user types
    };

    // Generate unique user ID and add to connected users
    let user_id = Uuid::new_v4().to_string();
    let connected_at = chrono::Utc::now()
        .format("%Y-%m-%d %H:%M:%S UTC")
        .to_string();

    let connected_user = ConnectedUser {
        user_type: user_type.clone(),
        user_id: user_id.clone(),
        connected_at,
        last_heartbeat: std::sync::Arc::new(std::sync::RwLock::new(chrono::Utc::now())),
    };

    // Add user to connected list
    session
        .connected_users
        .insert(user_id.clone(), connected_user);

    info!(
        "Web WebSocket connected for session: {} (user_type: {}, user_id: {}, readonly: {})",
        session_id, user_type, user_id, is_readonly
    );

    // Send user_id to client as first message
    let user_id_msg = serde_json::json!({
        "type": "user_id",
        "user_id": user_id
    })
    .to_string();

    if ws_sender
        .send(Message::Text(user_id_msg.into()))
        .await
        .is_err()
    {
        warn!(
            "Failed to send user_id to web client for session: {}",
            session_id
        );
        return;
    }

    // Send history to new web client
    let history_data = {
        let history_guard = session.history.read().await;
        history_guard.clone()
    };

    if !history_data.is_empty() {
        info!(
            "Sending {} bytes of history to new web client for session: {}",
            history_data.len(),
            session_id
        );
        if ws_sender
            .send(Message::Binary(history_data.into()))
            .await
            .is_err()
        {
            warn!(
                "Failed to send history to web client for session: {}",
                session_id
            );
            return;
        }
    }

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

    // Remove user from connected list on disconnection
    session.connected_users.remove(&user_id);

    info!(
        "Web WebSocket disconnected for session: {} (user_id: {})",
        session_id, user_id
    );
}
