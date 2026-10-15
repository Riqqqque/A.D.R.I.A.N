//! Adrian server library — state construction and the HTTP router.

pub mod api;
pub mod audio;
pub mod auth;
pub mod config;
pub mod db;
pub mod error;
pub mod ha;
pub mod memory;
pub mod providers;
pub mod scheduler;
pub mod secrets;
pub mod state;
pub mod tools;
pub mod util;
pub mod web;

pub use config::Config;
pub use state::AppState;

use axum::Router;
use std::sync::Arc;
use std::time::Duration;

/// Build the shared state: database worker, HTTP client, event bus, HA
/// manager, audio registry, scheduler handle.
pub async fn build_state(config: Config) -> Result<Arc<AppState>, error::ApiError> {
    std::fs::create_dir_all(&config.data_dir).map_err(error::ApiError::internal)?;
    let db = db::Db::open(&config.db_path(), 128).await?;
    let http = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .pool_idle_timeout(Duration::from_secs(90))
        .user_agent(concat!("adrian/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(error::ApiError::internal)?;
    let secret_box = Arc::new(secrets::SecretBox::load_or_create(
        &secrets::SecretBox::key_path(&config.data_dir),
    )?);
    let (events, _) = tokio::sync::broadcast::channel(256);
    let ha = ha::HaManager::new(db.clone(), http.clone(), events.clone(), secret_box.clone());
    let state = Arc::new(AppState {
        config,
        db,
        rate_limit: auth::ratelimit::RateLimiter::new().into(),
        http,
        events,
        scheduler: scheduler::SchedulerHandle::detached(),
        audio: audio::AudioRegistry::new().into(),
        satellites: api::satellites::SatelliteHub::new().into(),
        ha,
        secret_box,
        started: std::time::Instant::now(),
    });
    Ok(state)
}

/// Start background services (scheduler executor, HA WS supervisor) and the
/// one-use setup-token bootstrap.
pub async fn start_services(state: &Arc<AppState>) -> Result<(), error::ApiError> {
    auth::bootstrap(&state.db, &state.config.data_dir).await?;
    let sealed = state.seal_legacy_secrets().await?;
    if sealed > 0 {
        tracing::info!(count = sealed, "encrypted legacy plaintext secrets");
    }
    state.scheduler.clone().start(state.clone());
    scheduler::replicas::spawn_sync_on_changes(state.clone());
    Arc::new(state.ha.clone()).spawn();
    // Periodic housekeeping: expired/revoked sessions.
    let gc_db = state.db.clone();
    tokio::spawn(async move {
        loop {
            auth::sweep_expired_sessions(&gc_db).await;
            tokio::time::sleep(Duration::from_secs(3600)).await;
        }
    });
    Ok(())
}

pub fn build_router(state: Arc<AppState>) -> Router {
    let web_dir = state.config.web_dir.clone();
    Router::new()
        .merge(auth::routes())
        .merge(api::routes())
        .merge(web::router(&web_dir))
        .with_state(state.as_ref().clone())
}
