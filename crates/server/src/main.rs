//! Adrian server entry point.

use adrian_server::{Config, build_router, build_state, start_services};
use std::net::SocketAddr;
use std::time::Duration;
use tokio::net::TcpListener;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| "info,adrian=debug".into()),
        )
        .init();

    let config = match Config::from_env() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("bad configuration: {e}");
            std::process::exit(1);
        }
    };
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
    let service = app.into_make_service_with_connect_info::<SocketAddr>();

    if let Some(tls) = &config.tls {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        let rustls_config =
            match axum_server::tls_rustls::RustlsConfig::from_pem_file(&tls.cert, &tls.key).await {
                Ok(c) => c,
                Err(e) => {
                    eprintln!(
                        "cannot load TLS certificate {} / key {}: {e}",
                        tls.cert.display(),
                        tls.key.display()
                    );
                    std::process::exit(1);
                }
            };
        let handle = axum_server::Handle::new();
        let shutdown_handle = handle.clone();
        tokio::spawn(async move {
            shutdown().await;
            shutdown_handle.graceful_shutdown(Some(Duration::from_secs(10)));
        });
        announce(&config);
        axum_server::bind_rustls(config.bind, rustls_config)
            .handle(handle)
            .serve(service)
            .await
            .unwrap_or_else(|e| {
                eprintln!("cannot serve on {}: {e}", config.bind);
                std::process::exit(1);
            });
        return;
    }

    let listener = match TcpListener::bind(config.bind).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("cannot bind {}: {e}", config.bind);
            std::process::exit(1);
        }
    };
    announce(&config);
    axum::serve(listener, service)
        .with_graceful_shutdown(shutdown())
        .await
        .unwrap_or_else(|e| tracing::error!(error = %e, "server error"));
}

fn announce(config: &Config) {
    eprintln!("Adrian listening on {}", config.base_url());
    if config.bind.ip().is_loopback() {
        eprintln!("(loopback only — set ADRIAN_BIND=0.0.0.0:8787 for LAN access)");
    } else if config.tls.is_none() {
        eprintln!(
            "(plain HTTP on the LAN — set ADRIAN_TLS_CERT/ADRIAN_TLS_KEY or use a TLS reverse proxy)"
        );
    }
}

async fn shutdown() {
    let _ = tokio::signal::ctrl_c().await;
    tracing::info!("shutting down");
}
