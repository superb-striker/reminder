//! Router assembly and authentication.
//!
//! # Auth model
//!
//! A single-user personal server doesn't need OAuth flows or token
//! rotation: one long-lived bearer token, generated once and stored in
//! an environment variable on the server (and in each client's config),
//! is enough. Every route except `/health` requires
//! `Authorization: Bearer <token>` matching it.
//!
//! No external crate (`subtle`, etc.) is pulled in just for the
//! constant-time comparison -- [`constant_time_eq`] is four lines and
//! doesn't justify a dependency.
//!
//! # TLS
//!
//! This server speaks plain HTTP and is meant to sit behind a reverse
//! proxy (nginx/caddy with Let's Encrypt) that terminates TLS -- see
//! Phase 9 in the project brief. Baking TLS into the Rust binary
//! itself would mean carrying a certificate-reload story in
//! application code for no real benefit over a proxy that already
//! does this well.

pub mod routes;

use crate::db::Db;
use axum::{
    extract::{Request, State},
    http::{header, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
    Router,
};
use std::sync::Arc;

#[derive(Clone)]
pub struct AppState {
    pub db: Db,
    pub token: Arc<str>,
}

/// Build the full router: `/health` open, everything else behind the
/// bearer-token auth middleware.
pub fn build_router(db: Db, token: String) -> Router {
    let state = AppState {
        db,
        token: Arc::from(token),
    };

    let protected = Router::new()
        .route(
            "/reminders",
            get(routes::list_reminders).post(routes::create_reminder),
        )
        .route(
            "/reminders/:id",
            get(routes::get_reminder)
                .put(routes::update_reminder)
                .delete(routes::delete_reminder),
        )
        .route("/reminders/:id/complete", post(routes::complete_reminder))
        .route(
            "/occurrences/pending-laptop",
            get(routes::pending_laptop_occurrences),
        )
        .route(
            "/occurrences/:id/ack-laptop",
            post(routes::ack_laptop_occurrence),
        )
        .layer(middleware::from_fn_with_state(state.clone(), auth));

    Router::new()
        .route("/health", get(routes::health))
        .merge(protected)
        .with_state(state)
}

async fn auth(State(state): State<AppState>, req: Request, next: Next) -> Response {
    let provided_token = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));

    match provided_token {
        Some(tok) if constant_time_eq(tok.as_bytes(), state.token.as_bytes()) => {
            next.run(req).await
        }
        _ => (StatusCode::UNAUTHORIZED, "missing or invalid bearer token").into_response(),
    }
}

/// Compare two byte strings in time that doesn't depend on where they
/// first differ, so a timing side-channel can't be used to guess the
/// token one byte at a time.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constant_time_eq_matches_equal_slices() {
        assert!(constant_time_eq(b"same-token", b"same-token"));
    }

    #[test]
    fn constant_time_eq_rejects_different_slices() {
        assert!(!constant_time_eq(b"token-a", b"token-b"));
    }

    #[test]
    fn constant_time_eq_rejects_different_lengths() {
        assert!(!constant_time_eq(b"short", b"much-longer-token"));
    }
}
