//! HTTP API surface. Authenticated handlers take an `AuthUser` extractor;
//! public endpoints are limited to setup/login and tokenized media.

pub mod alarms;
pub mod chat;
pub mod devices;
pub mod diagnostics;
pub mod endpoints;
pub mod events;
pub mod integrations;
pub mod media;
pub mod memory_api;
pub mod satellites;
pub mod settings;
pub mod usage_api;

use crate::state::AppState;
use axum::Router;

pub fn routes() -> Router<AppState> {
    Router::new()
        .merge(settings::routes())
        .merge(integrations::routes())
        .merge(devices::routes())
        .merge(memory_api::routes())
        .merge(alarms::routes())
        .merge(endpoints::routes())
        .merge(chat::routes())
        .merge(satellites::routes())
        .merge(events::routes())
        .merge(diagnostics::routes())
        .merge(media::routes())
        .merge(usage_api::routes())
}
