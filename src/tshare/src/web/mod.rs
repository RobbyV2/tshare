use anyhow::Result;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{Html, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use clap::Parser;
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};

use tokio_tungstenite::{connect_async, tungstenite::Message as TungsteniteMessage};
use tracing::{error, info};

// Embed HTML templates at compile time
const INDEX_HTML: &str = include_str!("../../../../public/index.html");
const CONNECT_HTML: &str = include_str!("../../../../public/connect.html");

#[derive(Clone)]
struct AppState {
    tunnel_base: String,
}

#[derive(Serialize, Deserialize, Clone)]
struct SessionDetails {
    session_id: String,
    owner_password_hash: Option<String>,
    guest_password_hash: Option<String>,
    is_guest_readonly: bool,
}

#[derive(Deserialize)]
struct AuthQuery {
    password: Option<String>,
}

#[derive(Deserialize)]
struct WebSocketQuery {
    user_type: Option<String>,
}

#[derive(Parser, Debug)]
#[command(
    author,
    version,
    about = "TShare web server - serves terminal sessions via web interface"
)]
pub struct Args {
    /// Host to bind the web server to
    #[arg(long, default_value = "127.0.0.1")]
    pub host: String,

    /// Port for the web server
    #[arg(long, default_value_t = 8386)]
    pub port: u16,

    /// Tunnel server base URL
    #[arg(long, default_value = "http://127.0.0.1:8385")]
    pub tunnel_url: String,
}

pub async fn run_web_server(args: Args) -> Result<()> {
    // Validate connection to tunnel server before starting
    info!(
        "Validating connection to tunnel server at: {}",
        args.tunnel_url
    );
    let client = reqwest::Client::new();
    let test_url = format!("{}/api/session/connection-test", args.tunnel_url);

    match client.get(&test_url).send().await {
        Ok(response) => {
            // We expect 404 for a non-existent session, which means the server is running
            if response.status() == 404 || response.status().is_success() {
                info!("Successfully validated tunnel server connection");
            } else {
                error!(
                    "Tunnel server returned unexpected status: {}",
                    response.status()
                );
                panic!(
                    "Cannot start web server: tunnel server is not responding properly at {}",
                    args.tunnel_url
                );
            }
        }
        Err(e) => {
            error!("Failed to connect to tunnel server: {}", e);
            panic!(
                "Cannot start web server: tunnel server is unreachable at {}",
                args.tunnel_url
            );
        }
    }

    let app_state = AppState {
        tunnel_base: args.tunnel_url,
    };

    let app = Router::new()
        .route("/", get(home_page))
        .route("/session/{id}", get(session_page))
        .route("/ws/session/{id}", get(handle_session_ws))
        .route("/api/session/{id}", get(get_session_details))
        .route("/api/session/{id}/users", get(get_connected_users))
        .route(
            "/api/session/{session_id}/heartbeat/{user_id}",
            post(send_heartbeat),
        )
        .route("/api/auth/{id}", get(authenticate_session))
        .route("/api/logout", post(logout_session))
        .with_state(app_state);

    let addr = format!("{}:{}", args.host, args.port);
    let listener = tokio::net::TcpListener::bind(&addr).await?;

    info!("Web server starting on http://{}", addr);

    axum::serve(listener, app).await?;

    Ok(())
}

async fn home_page() -> Html<String> {
    Html(INDEX_HTML.to_string())
}

async fn session_page(
    Path(session_id): Path<String>,
    State(state): State<AppState>,
) -> Result<Html<String>, StatusCode> {
    // Check if session exists
    let client = reqwest::Client::new();
    let url = format!("{}/api/session/{}", state.tunnel_base, session_id);

    let response = client
        .get(&url)
        .send()
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    if !response.status().is_success() {
        return Err(StatusCode::NOT_FOUND);
    }

    let details: SessionDetails = response
        .json()
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let needs_auth = details.owner_password_hash.is_some() || details.guest_password_hash.is_some();

    let html = CONNECT_HTML
        .replace("{{ session_id }}", &session_id)
        .replace(
            "{{ modal_display }}",
            if needs_auth { "" } else { "hidden" },
        )
        .replace(
            "{{ terminal_display }}",
            if needs_auth { "hidden" } else { "" },
        )
        .replace(
            "{{ needs_auth_js }}",
            if needs_auth { "true" } else { "false" },
        );

    Ok(Html(html))
}

async fn get_session_details(
    Path(session_id): Path<String>,
    State(state): State<AppState>,
) -> Result<Json<SessionDetails>, StatusCode> {
    let client = reqwest::Client::new();
    let url = format!("{}/api/session/{}", state.tunnel_base, session_id);

    let response = client
        .get(&url)
        .send()
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    if !response.status().is_success() {
        return Err(StatusCode::NOT_FOUND);
    }

    let details: SessionDetails = response
        .json()
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok(Json(details))
}

async fn get_connected_users(
    Path(session_id): Path<String>,
    State(state): State<AppState>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let client = reqwest::Client::new();
    let url = format!("{}/api/session/{}/users", state.tunnel_base, session_id);

    let response = client
        .get(&url)
        .send()
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    if !response.status().is_success() {
        return Err(StatusCode::NOT_FOUND);
    }

    let users: serde_json::Value = response
        .json()
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok(Json(users))
}

async fn send_heartbeat(
    Path((session_id, user_id)): Path<(String, String)>,
    State(state): State<AppState>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let client = reqwest::Client::new();
    let url = format!(
        "{}/api/session/{}/heartbeat/{}",
        state.tunnel_base, session_id, user_id
    );

    let response = client
        .post(&url)
        .send()
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    if response.status().is_success() {
        Ok(Json(serde_json::json!({"success": true})))
    } else {
        Err(StatusCode::INTERNAL_SERVER_ERROR)
    }
}

async fn authenticate_session(
    Path(session_id): Path<String>,
    Query(auth): Query<AuthQuery>,
    State(state): State<AppState>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let client = reqwest::Client::new();
    let url = format!("{}/api/session/{}", state.tunnel_base, session_id);

    let response = client
        .get(&url)
        .send()
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    if !response.status().is_success() {
        return Err(StatusCode::NOT_FOUND);
    }

    let details: SessionDetails = response
        .json()
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let password = auth.password.unwrap_or_default();
    let mut authenticated = false;
    let mut user_type = "guest";

    // If no passwords are set, authenticate automatically as owner
    if details.owner_password_hash.is_none() && details.guest_password_hash.is_none() {
        authenticated = true;
        user_type = "owner";
    } else {
        // Check owner password first, then guest password
        if let Some(owner_hash) = &details.owner_password_hash {
            if bcrypt::verify(&password, owner_hash).unwrap_or(false) {
                authenticated = true;
                user_type = "owner";
            }
        }

        if !authenticated {
            if let Some(guest_hash) = &details.guest_password_hash {
                if bcrypt::verify(&password, guest_hash).unwrap_or(false) {
                    authenticated = true;
                    user_type = "guest";
                }
            }
        }
    }

    let is_readonly = match user_type {
        "owner" => false,
        "guest" => details.is_guest_readonly,
        _ => true,
    };

    Ok(Json(serde_json::json!({
        "authenticated": authenticated,
        "user_type": user_type,
        "is_readonly": is_readonly
    })))
}

async fn logout_session() -> Result<Json<serde_json::Value>, StatusCode> {
    Ok(Json(serde_json::json!({
        "success": true,
        "message": "Logged out successfully"
    })))
}

async fn handle_session_ws(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    Query(query): Query<WebSocketQuery>,
    ws: WebSocketUpgrade,
) -> Response {
    let user_type = query.user_type.unwrap_or_else(|| "guest".to_string());
    ws.on_upgrade(move |socket| proxy_to_tunnel_server(socket, session_id, state, user_type))
}

async fn proxy_to_tunnel_server(
    client_socket: WebSocket,
    session_id: String,
    state: AppState,
    user_type: String,
) {
    let tunnel_url = format!(
        "{}/ws/web/{}?user_type={}",
        state.tunnel_base.replace("http", "ws"),
        session_id,
        user_type
    );

    info!("Attempting to connect to tunnel server: {}", tunnel_url);

    match connect_async(&tunnel_url).await {
        Ok((tunnel_stream, _)) => {
            let (mut tunnel_sink, mut tunnel_stream) = tunnel_stream.split();
            let (mut client_sink, mut client_stream) = client_socket.split();

            info!("Proxying WebSocket for session: {}", session_id);

            let client_to_tunnel = tokio::spawn(async move {
                while let Some(msg) = client_stream.next().await {
                    match msg {
                        Ok(Message::Binary(data)) => {
                            if tunnel_sink
                                .send(TungsteniteMessage::Binary(data))
                                .await
                                .is_err()
                            {
                                break;
                            }
                        }
                        Ok(Message::Text(text)) => {
                            if tunnel_sink
                                .send(TungsteniteMessage::Text(text.to_string().into()))
                                .await
                                .is_err()
                            {
                                break;
                            }
                        }
                        Ok(Message::Close(_)) => break,
                        Err(_) => break,
                        _ => {}
                    }
                }
            });

            let tunnel_to_client = tokio::spawn(async move {
                while let Some(msg) = tunnel_stream.next().await {
                    match msg {
                        Ok(TungsteniteMessage::Binary(data)) => {
                            if client_sink.send(Message::Binary(data)).await.is_err() {
                                break;
                            }
                        }
                        Ok(TungsteniteMessage::Text(text)) => {
                            if client_sink
                                .send(Message::Text(text.to_string().into()))
                                .await
                                .is_err()
                            {
                                break;
                            }
                        }
                        Ok(TungsteniteMessage::Close(_)) => break,
                        Err(_) => break,
                        _ => {}
                    }
                }
            });

            tokio::select! {
                _ = client_to_tunnel => {},
                _ = tunnel_to_client => {},
            }
        }
        Err(e) => {
            error!("Failed to connect to tunnel server: {}", e);
        }
    }

    info!("WebSocket proxy ended for session: {}", session_id);
}
