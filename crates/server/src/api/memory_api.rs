//! Explicit-memory HTTP surface: list/search/create/edit/delete, scoped by
//! the caller's visibility rules in the DB queries.

use crate::auth::AuthUser;
use crate::error::ApiError;
use crate::memory;
use crate::state::AppState;
use axum::extract::{Path, Query, State};
use axum::routing::{get, put};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/memory", get(list).post(create))
        .route("/api/memory/{id}", put(update).delete(remove))
}

#[derive(Deserialize)]
struct ListQuery {
    q: Option<String>,
}

async fn list(
    user: AuthUser,
    State(s): State<AppState>,
    Query(q): Query<ListQuery>,
) -> Result<Json<Value>, ApiError> {
    let facts = match q.q.as_deref().map(str::trim).filter(|q| !q.is_empty()) {
        Some(query) => memory::search(&s.db, user.id, query, 50).await?,
        None => memory::list(&s.db, user.id).await?,
    };
    Ok(Json(json!({"facts": facts})))
}

#[derive(Deserialize)]
struct CreateBody {
    text: String,
    scope: Option<String>,
}

async fn create(
    user: AuthUser,
    State(s): State<AppState>,
    Json(b): Json<CreateBody>,
) -> Result<Json<Value>, ApiError> {
    let scope = b.scope.as_deref().unwrap_or("private");
    let scope = if user.role == "guest" {
        "private"
    } else {
        scope
    };
    let fact = memory::add(&s.db, user.id, &b.text, scope, "web_ui").await?;
    s.emit("memory", json!({"event": "created", "id": fact.id}));
    Ok(Json(json!({"ok": true, "fact": fact})))
}

#[derive(Deserialize)]
struct UpdateBody {
    text: String,
}

async fn update(
    user: AuthUser,
    State(s): State<AppState>,
    Path(id): Path<i64>,
    Json(b): Json<UpdateBody>,
) -> Result<Json<Value>, ApiError> {
    let fact = memory::update(&s.db, id, user.id, &b.text).await?;
    s.emit("memory", json!({"event": "updated", "id": id}));
    Ok(Json(json!({"ok": true, "fact": fact})))
}

async fn remove(
    user: AuthUser,
    State(s): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Json<Value>, ApiError> {
    memory::delete(&s.db, id, user.id).await?;
    s.emit("memory", json!({"event": "deleted", "id": id}));
    Ok(Json(json!({"ok": true})))
}
