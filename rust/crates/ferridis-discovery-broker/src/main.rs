//! Entry point for the `ferridis-discovery-broker` binary.
//!
//! All business logic lives in the library crate (`lib.rs` → `store`, `routes`).

use clap::Parser;
use ferridis_discovery_broker::{routes, state_path, store::ServiceStore};
use std::path::PathBuf;
use tracing_subscriber::EnvFilter;

/// Ferridis discovery broker — federated service registry.
#[derive(Parser)]
#[command(name = "ferridis-discovery-broker")]
struct Args {
    /// Address to bind (host:port).
    #[arg(long, default_value = "127.0.0.1:7825")]
    bind: std::net::SocketAddr,
    /// Path to the JSON state file for persistent registrations.
    ///
    /// Registrations are written atomically after every `POST /discovery/register`
    /// and reloaded at startup so they survive process restarts.
    /// Defaults to `$HOME/.local/share/ferridis/discovery.json`, or
    /// `%LOCALAPPDATA%\Ferridis\discovery.json` when `HOME` is unset (a
    /// Windows service). With neither set the broker refuses to start.
    #[arg(long)]
    state_file: Option<PathBuf>,
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .init();

    let args = Args::parse();

    let state_path = match args.state_file {
        Some(path) => path,
        None => match state_path::default_state_path(|k| std::env::var(k).ok()) {
            Ok(path) => path,
            Err(e) => {
                eprintln!("ferridis-discovery-broker: {e}");
                std::process::exit(2);
            }
        },
    };

    let store = ServiceStore::with_state(state_path.clone()); // clone: path kept for log message

    match store.load_state().await {
        Ok(0) => tracing::info!(
            path = %state_path.display(),
            "state file absent or empty — starting fresh"
        ),
        Ok(n) => tracing::info!(
            count = n,
            path = %state_path.display(),
            "restored registrations from state file"
        ),
        Err(e) => tracing::warn!(
            error = %e,
            path = %state_path.display(),
            "failed to load state file — starting with empty registry"
        ),
    }

    let app = routes::make_router(store);

    tracing::info!("ferridis-discovery-broker listening on {}", args.bind);

    let listener = tokio::net::TcpListener::bind(args.bind)
        .await
        .expect("bind failed"); // allow:unwrap startup — unrecoverable if bind fails
    axum::serve(listener, app).await.expect("server error"); // allow:unwrap startup — unrecoverable if serve fails
}
