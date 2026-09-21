use std::sync::Arc;

use axum::{
    Json,
    body::Body,
    extract::{State, rejection::JsonRejection},
    http::{StatusCode, header},
    response::Response,
};
use futures_util::StreamExt;
use serde_json::value::RawValue;

use crate::{
    App, body,
    protocol::{ApiError, ApiResult},
};

/// The Agents SDK owns its runner and uses this model transport endpoint.
/// The session endpoints run the separate, server-owned NanoCodex lifecycle.
pub async fn responses(
    State(app): State<Arc<App>>,
    request: Result<Json<Box<RawValue>>, JsonRejection>,
) -> ApiResult<Response> {
    let request = body(request)?;
    let permit = app.slots.clone().try_acquire_owned().map_err(|_| {
        ApiError(
            StatusCode::TOO_MANY_REQUESTS,
            "rate_limit_error",
            "All agent slots are in use".into(),
        )
    })?;
    let response = app
        .http
        .post(format!("{}/responses", app.model_url.trim_end_matches('/')))
        .bearer_auth(&app.model_key)
        .header(header::CONTENT_TYPE, "application/json")
        .body(request.get().to_owned())
        .send()
        .await
        .map_err(|_| {
            ApiError(
                StatusCode::BAD_GATEWAY,
                "server_error",
                "Model provider request failed".into(),
            )
        })?;
    let status = response.status();
    let content_type = response.headers().get(header::CONTENT_TYPE).cloned();
    let stream = response.bytes_stream().map(move |chunk| {
        let _keep_slot = &permit;
        chunk
    });
    let mut reply = Response::new(Body::from_stream(stream));
    *reply.status_mut() = status;
    if let Some(content_type) = content_type {
        reply
            .headers_mut()
            .insert(header::CONTENT_TYPE, content_type);
    }
    Ok(reply)
}
