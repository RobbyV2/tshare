//! # TShare - Terminal Sharing Application
//!
//! TShare allows you to share your terminal session with others through a web interface.
//! The application consists of three main components that work together:
//!
//! ## Architecture Overview
//!
//! ```text
//!    Client              Tunnel Server           Web Server
//! ┌─────────────┐      ┌─────────────────┐     ┌─────────────────┐
//! │  Terminal   │◄────►│  WebSocket      │◄───►│  Web Interface  │
//! │  Session    │      │  Coordination   │     │  (HTML/JS)      │
//! │  (PTY)      │      │  Hub            │     │                 │
//! └─────────────┘      └─────────────────┘     └─────────────────┘
//! ```
//!
//! ## Components
//!
//! - **Client**: Creates and manages terminal sessions using PTY, forwards terminal
//!   I/O to the tunnel server via WebSocket
//! - **Tunnel Server**: Central coordination hub that manages active sessions and
//!   routes data between terminal clients and web viewers
//! - **Web Server**: Serves the web interface for viewing and interacting with
//!   shared terminal sessions
//!
//! ## Usage Examples
//!
//! Start servers:
//! ```bash
//! # Terminal 1: Start tunnel server
//! tshare tunnel --host 0.0.0.0 --port 8385
//!
//! # Terminal 2: Start web server  
//! tshare web --host 0.0.0.0 --port 8386 --tunnel-url http://localhost:8385
//! ```
//!
//! Share a terminal:
//! ```bash
//! # Creates session and displays shareable web link
//! tshare connect --tunnel-host localhost --web-host localhost
//!
//! # With password protection
//! tshare connect --owner-pass secret123 --guest-pass view123 --guest-readonly
//! ```

use anyhow::Result;
use clap::{Parser, Subcommand};
use tracing::info;

mod client;
mod tunnel;
mod web;

/// Command-line arguments for the TShare application
#[derive(Parser, Debug)]
#[command(author, version, about = "Share your terminal session via a web link.")]
struct Args {
    #[command(subcommand)]
    command: Commands,
}

/// Available subcommands for TShare
///
/// Each command serves a different role in the terminal sharing system:
/// - `connect`: Creates a new terminal session for sharing
/// - `tunnel`: Runs the coordination server that manages sessions  
/// - `web`: Runs the web server that serves the browser interface
#[derive(Subcommand, Debug)]
enum Commands {
    /// Connect and start a new terminal sharing session.
    ///
    /// This creates a PTY (pseudo-terminal) with your default shell and connects
    /// it to the tunnel server. The terminal I/O is forwarded through WebSocket
    /// to allow web users to view and optionally interact with the session.
    ///
    /// # Example
    /// ```bash
    /// tshare connect --tunnel-host example.com --owner-pass mysecret
    /// ```
    Connect(client::Args),

    /// Start the tunnel server that handles PTY data streams.
    ///
    /// The tunnel server is the central coordination hub that:
    /// - Manages active terminal sessions
    /// - Routes data between terminal clients and web viewers
    /// - Handles user authentication and permissions
    /// - Maintains session history for late-joining viewers
    ///
    /// # Example
    /// ```bash
    /// tshare tunnel --host 0.0.0.0 --port 8385
    /// ```
    Tunnel(tunnel::Args),

    /// Start the web server that serves terminal sessions via web interface.
    ///
    /// The web server provides the browser-based interface for viewing shared
    /// terminals. It serves HTML/CSS/JS assets and proxies WebSocket connections
    /// to the tunnel server.
    ///
    /// # Example
    /// ```bash
    /// tshare web --host 0.0.0.0 --port 8386 --tunnel-url http://localhost:8385
    /// ```
    Web(web::Args),
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    // Create ~/.tshare directory if it doesn't exist
    let home_dir = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    let tshare_dir = format!("{home_dir}/.tshare");
    std::fs::create_dir_all(&tshare_dir)?;

    // Determine log file based on subcommand
    let (log_path, console_output) = match &args.command {
        Commands::Connect(_) => (format!("{tshare_dir}/tshare.log"), false), // No console output for connect
        Commands::Tunnel(_) => (format!("{tshare_dir}/tshare-tunnel.log"), true), // Console output for servers
        Commands::Web(_) => (format!("{tshare_dir}/tshare-web.log"), true),
    };

    // Clear previous log file
    let _ = std::fs::remove_file(&log_path);

    // Configure logging based on subcommand
    let log_file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&log_path)?;

    if console_output {
        // For servers: log to both console and file
        use tracing_subscriber::fmt::writer::MakeWriterExt;
        let writer = std::io::stdout.and(log_file);

        tracing_subscriber::fmt()
            .with_env_filter(
                tracing_subscriber::EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
            )
            .with_writer(writer)
            .init();
    } else {
        // For connect: only log to file
        tracing_subscriber::fmt()
            .with_env_filter(
                tracing_subscriber::EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
            )
            .with_writer(log_file)
            .with_ansi(false)
            .init();
    }

    // Log startup message based on subcommand
    match &args.command {
        Commands::Connect(_) => {
            info!("Starting tshare client");
        }
        Commands::Tunnel(_) => {
            info!("Starting tshare tunnel server");
        }
        Commands::Web(_) => {
            info!("Starting tshare web server");
        }
    }

    match args.command {
        Commands::Connect(client_args) => client::run_client(client_args).await,
        Commands::Tunnel(tunnel_args) => tunnel::run_tunnel_server(tunnel_args).await,
        Commands::Web(web_args) => web::run_web_server(web_args).await,
    }
}
