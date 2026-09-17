//! Adrian server entry point.

use adrian_server::{Config, build_router, build_state, start_services};
use std::net::SocketAddr;
use tokio::net::TcpListener;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| "info,adrian=debug".into()),
        )
        .init();

    let config = Config::from_env();
    let state = match build_state(config.clone()).await {
        Ok(s) => s,
        Err(e) => {
            eprintln!("failed to initialize: {e}");
            std::process::exit(1);
        }
    };
    if let Err(e) = start_services(&state).await {
        eprintln!("failed to start services: {e}");
        std::process::exit(1);
    }

    let app = build_router(state.clone());
    let listener = match TcpListener::bind(config.bind).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("cannot bind {}: {e}", config.bind);
            std::process::exit(1);
        }
    };
    eprintln!("Adrian listening on http://{}", config.bind);
    if config.bind.ip().is_loopback() {
        eprintln!("(loopback only — set ADRIAN_BIND=0.0.0.0:8787 for LAN access)");
    }
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown())
    .await
    .unwrap_or_else(|e| tracing::error!(error = %e, "server error"));
}

async fn shutdown() {
    let _ = tokio::signal::ctrl_c().await;
    tracing::info!("shutting down");
}
