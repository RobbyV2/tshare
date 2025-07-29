use anyhow::{Result, ensure};
use clap::Parser;
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use futures_util::{SinkExt, StreamExt};
use portable_pty::{CommandBuilder, PtySize};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::io::{Read, Write};
use tokio::select;
use tokio::sync::mpsc;
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{error, info};

#[derive(Parser, Debug)]
#[command(
    author,
    version,
    about = "TShare client - connect and start a new terminal sharing session"
)]
pub struct Args {
    /// Tunnel server host
    #[arg(long, default_value = "127.0.0.1")]
    pub tunnel_host: String,

    /// Tunnel server port
    #[arg(long, default_value_t = 8385)]
    pub tunnel_port: u16,

    /// Web server host
    #[arg(long, default_value = "127.0.0.1")]
    pub web_host: String,

    /// Web server port
    #[arg(long, default_value_t = 8386)]
    pub web_port: u16,

    /// Set a password required for the session owner to connect via the web.
    /// Owners always have full read/write access to the terminal.
    #[arg(long)]
    pub owner_pass: Option<String>,

    /// Set a password for guests to connect via the web.
    /// Guests follow the readonly setting below.
    #[arg(long)]
    pub guest_pass: Option<String>,

    /// Make guest sessions read-only (no input from web is forwarded).
    /// If true, guests can only view terminal output. If false, guests can interact.
    /// Owners always have full access regardless of this setting.
    #[arg(long, default_value_t = false)]
    pub guest_readonly: bool,
}

#[derive(Serialize)]
struct CreateSessionRequest {
    owner_password: Option<String>,
    guest_password: Option<String>,
    is_guest_readonly: bool,
}

#[derive(Deserialize)]
struct CreateSessionResponse {
    session_id: String,
}

pub async fn run_client(args: Args) -> Result<()> {
    let api_url = format!(
        "http://{}:{}/api/session",
        args.tunnel_host, args.tunnel_port
    );
    let web_addr = format!("http://{}:{}", args.web_host, args.web_port);

    // Validate connection to tunnel server before creating session
    info!(
        "Validating connection to tunnel server at: {}:{}",
        args.tunnel_host, args.tunnel_port
    );
    let client = Client::new();
    let test_url = format!(
        "http://{}:{}/api/session/connection-test",
        args.tunnel_host, args.tunnel_port
    );

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
                    "Cannot start client: tunnel server is not responding properly at {}:{}",
                    args.tunnel_host, args.tunnel_port
                );
            }
        }
        Err(e) => {
            error!("Failed to connect to tunnel server: {}", e);
            panic!(
                "Cannot start client: tunnel server is unreachable at {}:{}",
                args.tunnel_host, args.tunnel_port
            );
        }
    }

    info!("Registering session with tunnel server at: {}", api_url);

    // Register session with tunnel server
    let create_request = CreateSessionRequest {
        owner_password: args.owner_pass.clone(),
        guest_password: args.guest_pass.clone(),
        is_guest_readonly: args.guest_readonly,
    };

    let response = client.post(&api_url).json(&create_request).send().await?;

    ensure!(
        response.status().is_success(),
        "Failed to create session: {}",
        response.status()
    );

    let create_response: CreateSessionResponse = response.json().await?;
    let session_id = create_response.session_id;

    info!("Session created successfully: {}", session_id);

    // Display shareable link
    println!("=== TShare Session Created ===");
    println!("Session ID: {session_id}");
    println!("Share this link: {web_addr}/session/{session_id}");
    println!("==============================");
    println!("Starting shared terminal session...");
    println!("This terminal will be shared with viewers.");
    println!("Either exit the terminal or end the process to end the session.");
    println!("==============================");
    println!();

    // Spawn PTY with user's shell
    let pty_system = portable_pty::native_pty_system();
    let pty_pair = pty_system.openpty(PtySize {
        rows: 30,
        cols: 120,
        pixel_width: 0,
        pixel_height: 0,
    })?;

    let mut cmd = CommandBuilder::new(get_user_shell());
    cmd.env("TERM", "xterm-256color");
    cmd.env("COLORTERM", "truecolor");
    let mut child = pty_pair.slave.spawn_command(cmd)?;
    let master = pty_pair.master;

    // Connect to tunnel server WebSocket
    let ws_url = format!(
        "ws://{}:{}/ws/pty/{}",
        args.tunnel_host, args.tunnel_port, session_id
    );
    info!("Connecting to tunnel server WebSocket: {}", ws_url);

    let (ws_stream, _) = connect_async(&ws_url).await?;
    info!("Connected to tunnel server WebSocket successfully");
    let (mut ws_sink, mut ws_stream) = ws_stream.split();

    // Create channels for communication
    let (pty_output_tx, mut pty_output_rx) = mpsc::channel::<Vec<u8>>(1024);
    let (ws_input_tx, mut ws_input_rx) = mpsc::channel::<Vec<u8>>(1024);

    // Task to read from PTY and send to both WebSocket and stdout
    let master_reader = master.try_clone_reader().unwrap();
    let pty_output_tx_clone = pty_output_tx.clone();
    let pty_reader_task = tokio::task::spawn_blocking(move || {
        let mut reader = master_reader;
        let mut buffer = [0u8; 1024];
        loop {
            match reader.read(&mut buffer) {
                Ok(n) if n > 0 => {
                    let data = buffer[..n].to_vec();

                    // Send to WebSocket
                    if pty_output_tx_clone.blocking_send(data.clone()).is_err() {
                        break;
                    }

                    // Also write to stdout so user can see what's happening
                    if std::io::stdout().write_all(&data).is_err() {
                        break;
                    }
                    let _ = std::io::stdout().flush();
                }
                Ok(_) => break, // EOF
                Err(_) => break,
            }
        }
    });

    // Create a unified channel for all input to PTY (stdin + web)
    let (pty_input_tx, mut pty_input_rx) = mpsc::channel::<Vec<u8>>(1024);
    let pty_input_tx_stdin = pty_input_tx.clone();
    let pty_input_tx_web = pty_input_tx;

    // Enable raw mode for real-time character input
    enable_raw_mode().unwrap_or_else(|e| {
        eprintln!("Failed to enable raw mode: {e}");
    });

    // Set up signal handler to disable raw mode on Ctrl+C
    ctrlc::set_handler(move || {
        let _ = disable_raw_mode();
        std::process::exit(0);
    })
    .unwrap_or_else(|e| {
        eprintln!("Failed to set signal handler: {e}");
    });

    // Task to read raw bytes from stdin and send to PTY input channel
    let stdin_reader_task = tokio::task::spawn_blocking(move || {
        let mut stdin = std::io::stdin();
        let mut buffer = [0u8; 1];

        while stdin.read_exact(&mut buffer).is_ok() {
            // Send each byte as-is, no interpretation
            let data = vec![buffer[0]];
            if pty_input_tx_stdin.blocking_send(data).is_err() {
                break;
            }
        }
    });

    // Task to write WebSocket input to PTY input channel
    let ws_input_writer_task = tokio::spawn(async move {
        while let Some(data) = ws_input_rx.recv().await {
            info!("Received {} bytes from web, forwarding to PTY", data.len());
            if pty_input_tx_web.send(data).await.is_err() {
                break;
            }
        }
    });

    // Task to handle all PTY input (from both stdin and web)
    let mut master_writer = master.take_writer().unwrap();
    let pty_input_task = tokio::task::spawn_blocking(move || {
        while let Some(data) = pty_input_rx.blocking_recv() {
            if master_writer.write_all(&data).is_err() {
                break;
            }
        }
    });

    // Create resize channel
    let (resize_tx, mut resize_rx) = mpsc::channel::<(u16, u16)>(1);

    // Task to read from WebSocket
    let ws_reader_task = tokio::spawn(async move {
        while let Some(msg) = ws_stream.next().await {
            match msg {
                Ok(Message::Binary(data)) => {
                    // Check if this is a resize command
                    if let Ok(data_str) = std::str::from_utf8(&data) {
                        if data_str.starts_with("RESIZE:") {
                            if let Some(size_part) = data_str.strip_prefix("RESIZE:") {
                                let parts: Vec<&str> = size_part.split(':').collect();
                                if parts.len() == 2 {
                                    if let (Ok(cols), Ok(rows)) =
                                        (parts[0].parse::<u16>(), parts[1].parse::<u16>())
                                    {
                                        if resize_tx.send((cols, rows)).await.is_err() {
                                            error!("Failed to send resize command");
                                        }
                                        continue;
                                    }
                                }
                            }
                        }
                    }

                    // Regular data for PTY input
                    if ws_input_tx.send(data.to_vec()).await.is_err() {
                        break;
                    }
                }
                Ok(Message::Close(_)) => break,
                Err(_) => break,
                _ => {} // Ignore other message types
            }
        }
    });

    // Task to check child process
    let mut child_task = tokio::task::spawn_blocking(move || child.wait());

    let session_id_for_logging = session_id.clone();

    // Main coordination loop
    loop {
        select! {
            // PTY output -> WebSocket
            Some(data) = pty_output_rx.recv() => {
                if ws_sink.send(Message::Binary(data.into())).await.is_err() {
                    break;
                }
            }

            // Handle resize commands
            Some((cols, rows)) = resize_rx.recv() => {
                let new_size = portable_pty::PtySize {
                    rows,
                    cols,
                    pixel_width: 0,
                    pixel_height: 0,
                };
                if let Err(e) = master.resize(new_size) {
                    error!("Failed to resize PTY: {}", e);
                } else {
                    info!("Resized PTY to {}x{}", cols, rows);
                }
            }

            // Check if child process has exited
            child_result = &mut child_task => {
                match child_result {
                    Ok(_) => {
                        println!("\nTerminal session ended.");
                        info!("Terminal session {} ended normally", session_id_for_logging);
                    }
                    Err(e) => {
                        println!("\nTerminal session error: {e}");
                        info!("Terminal session {} ended with error: {}", session_id_for_logging, e);
                    }
                }
                break;
            }

            else => break,
        }
    }

    // Cleanup
    pty_reader_task.abort();
    stdin_reader_task.abort();
    ws_input_writer_task.abort();
    pty_input_task.abort();
    ws_reader_task.abort();

    // Send close frame with code 1000 (normal closure)
    let close_frame = Message::Close(Some(tokio_tungstenite::tungstenite::protocol::CloseFrame {
        code: tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode::Normal,
        reason: "Terminal session ended".into(),
    }));
    let _ = ws_sink.send(close_frame).await;
    let _ = ws_sink.close().await;

    // Disable raw mode and clean up terminal state before exiting
    let _ = disable_raw_mode();

    // Reset cursor and clear any remaining output
    print!("\r\n");
    let _ = std::io::stdout().flush();

    // Force exit since stdin task might still be blocking
    std::process::exit(0);
}

fn get_user_shell() -> String {
    std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".to_string())
}
