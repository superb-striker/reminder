//! Request handlers. Deliberately thin: every handler does argument
//! extraction, calls one `db::` function, and maps the result to a
//! response -- all the actual logic already lives in `db.rs` and is
//! tested there. Nothing here is re-tested at the HTTP layer; these
//! functions have almost no branches of their own to get wrong.

use super::AppState;
use crate::model::{NewReminder, Reminder, ReminderPatch};
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};

/// Wraps any error into a 500 response, so handlers can just use `?`
/// on `anyhow::Result`. Handlers that need a different status (404,
/// mainly) return it explicitly instead of relying on this.
pub struct ApiError(anyhow::Error);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (StatusCode::INTERNAL_SERVER_ERROR, self.0.to_string()).into_response()
    }
}

impl<E: Into<anyhow::Error>> From<E> for ApiError {
    fn from(err: E) -> Self {
        ApiError(err.into())
    }
}

pub async fn health() -> &'static str {
    "ok"
}

pub async fn list_reminders(
    State(state): State<AppState>,
) -> Result<Json<Vec<Reminder>>, ApiError> {
    let reminders = crate::db::list_reminders(&state.db, false).await?;
    Ok(Json(reminders))
}

pub async fn create_reminder(
    State(state): State<AppState>,
    Json(new): Json<NewReminder>,
) -> Result<Json<Reminder>, ApiError> {
    let reminder = crate::db::create_reminder(&state.db, &new).await?;
    Ok(Json(reminder))
}

pub async fn get_reminder(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Response, ApiError> {
    match crate::db::get_reminder(&state.db, id).await? {
        Some(r) => Ok(Json(r).into_response()),
        None => Ok(StatusCode::NOT_FOUND.into_response()),
    }
}

pub async fn update_reminder(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(patch): Json<ReminderPatch>,
) -> Result<Response, ApiError> {
    match crate::db::update_reminder(&state.db, id, &patch).await? {
        Some(r) => Ok(Json(r).into_response()),
        None => Ok(StatusCode::NOT_FOUND.into_response()),
    }
}

pub async fn delete_reminder(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<StatusCode, ApiError> {
    if crate::db::delete_reminder(&state.db, id).await? {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Ok(StatusCode::NOT_FOUND)
    }
}

/// Mark a reminder's most recently fired occurrence as completed.
/// 404s if the reminder has never fired -- there's nothing to
/// complete yet.
pub async fn complete_reminder(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Response, ApiError> {
    match crate::db::latest_occurrence_for_reminder(&state.db, id).await? {
        Some(occ) => {
            crate::db::mark_completed(&state.db, occ.id).await?;
            Ok(StatusCode::NO_CONTENT.into_response())
        }
        None => Ok((
            StatusCode::NOT_FOUND,
            "reminder has no fired occurrence to complete",
        )
            .into_response()),
    }
}

/// Polled by `reminder watch` on the laptop (`client/watch.rs`):
/// occurrences waiting for local (`omarchy reminder`) delivery.
pub async fn pending_laptop_occurrences(
    State(state): State<AppState>,
) -> Result<Json<Vec<crate::model::PendingLaptopOccurrence>>, ApiError> {
    let occs = crate::db::occurrences_pending_laptop_with_title(&state.db).await?;
    Ok(Json(occs))
}

/// Called by `reminder watch` after it has fired `omarchy reminder`
/// locally for an occurrence, so the server stops returning it from
/// `pending_laptop_occurrences`.
pub async fn ack_laptop_occurrence(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<StatusCode, ApiError> {
    crate::db::mark_laptop_delivered(&state.db, id).await?;
    Ok(StatusCode::NO_CONTENT)
}
