//! # TShare - Terminal Sharing Library
//!
//! TShare is a terminal sharing application that allows you to share your terminal
//! session with others through a web interface. This library provides the core
//! components for building terminal sharing systems.
//!
//! ## Quick Start
//!
//! The easiest way to use TShare is through the command-line interface:
//!
//! ```bash
//! # Start the servers
//! tshare tunnel --host 0.0.0.0 --port 8385 &
//! tshare web --host 0.0.0.0 --port 8386 &
//!
//! # Share your terminal
//! tshare connect
//! ```
//!
//! ## Library Usage
//!
//! You can also use TShare components programmatically:
//!
//! ```rust,no_run
//! use tshare::client::{Args as ClientArgs, run_client};
//! use tshare::tunnel::{Args as TunnelArgs, run_tunnel_server};
//! use tshare::web::{Args as WebArgs, run_web_server};
//!
//! # async fn example() -> anyhow::Result<()> {
//! // Start tunnel server
//! let tunnel_args = TunnelArgs {
//!     host: "0.0.0.0".to_string(),
//!     port: 8385,
//! };
//! tokio::spawn(run_tunnel_server(tunnel_args));
//!
//! // Start web server
//! let web_args = WebArgs {
//!     host: "0.0.0.0".to_string(),
//!     port: 8386,
//!     tunnel_url: "http://localhost:8385".to_string(),
//! };
//! tokio::spawn(run_web_server(web_args));
//!
//! // Create a client session
//! let client_args = ClientArgs {
//!     tunnel_host: "localhost".to_string(),
//!     tunnel_port: 8385,
//!     web_host: "localhost".to_string(),
//!     web_port: 8386,
//!     owner_pass: Some("secret".to_string()),
//!     guest_pass: None,
//!     guest_readonly: false,
//! };
//! run_client(client_args).await?;
//! # Ok(())
//! # }
//! ```
//!
//! ## Architecture
//!
//! TShare consists of three main components:
//!
//! - **[Client](client)**: Creates terminal sessions and forwards I/O
//! - **[Tunnel Server](tunnel)**: Coordinates sessions and manages data routing  
//! - **[Web Server](web)**: Serves browser interface and proxies connections
//!
//! ## Security
//!
//! TShare supports password-based authentication with bcrypt hashing:
//!
//! - **Owner passwords**: Full read/write access to terminals
//! - **Guest passwords**: Configurable read-only or read/write access
//! - **No authentication**: First user gets owner privileges
//!
//! ## Performance
//!
//! - **Concurrent connections**: Supports multiple viewers per session
//! - **Efficient broadcasting**: Uses tokio broadcast channels for data distribution
//! - **Memory management**: Bounded history buffers prevent unlimited growth
//! - **Real-time streaming**: WebSocket-based for low-latency terminal interaction

pub mod client;
pub mod tunnel;
pub mod web;
