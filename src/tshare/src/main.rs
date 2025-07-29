use anyhow::Result;
use clap::{Parser, Subcommand};
use tracing::info;

mod client;
mod tunnel;
mod web;

#[derive(Parser, Debug)]
#[command(author, version, about = "Share your terminal session via a web link.")]
struct Args {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Connect and start a new terminal sharing session.
    Connect(client::Args),
    /// Start the tunnel server that handles PTY data streams.
    Tunnel(tunnel::Args),
    /// Start the web server that serves terminal sessions via web interface.
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
