//! Server-event stream (entity state changes, alarm lifecycle, HA status)
//! for authenticated UI clients. Cookie auth via GET.

use crate::auth::AuthUser;
use crate::state::{AppState, ServerEvent};
use axum::Router;
use axum::extract::State;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::routing::get;
use std::convert::Infallible;
use std::time::Duration;

pub fn routes() -> Router<AppState> {
    Router::new().route("/api/events", get(stream))
}

/// Light state is for the owner and for lights the household may use.
fn visible(ev: &ServerEvent, user: &AuthUser) -> bool {
    ev.kind != "entity_state" || user.is_owner() || ev.data["allowed"] == true
}

async fn stream(
    user: AuthUser,
    State(s): State<AppState>,
) -> Sse<impl futures_util::Stream<Item = Result<Event, Infallible>>> {
    let mut rx = s.events.subscribe();
    let stream = async_stream::stream! {
        yield Ok(Event::default().event("hello").data("{}"));
        loop {
            match rx.recv().await {
                Ok(ev) if !visible(&ev, &user) => continue,
                Ok(ev) => {
                    let data = serde_json::to_string(&ev.data).unwrap_or_default();
                    yield Ok(Event::default().event(ev.kind).data(data));
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(_) => break,
            }
        }
    };
    Sse::new(stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(25)))
}
