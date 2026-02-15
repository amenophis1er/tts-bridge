use std::time::Duration;

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde_json::json;

use crate::AppState;

pub async fn health_check(State(state): State<AppState>) -> impl IntoResponse {
    let backend_url = &state.config.backend_url;

    let health_url = format!("{backend_url}/health");
    let models_url = format!("{backend_url}/v1/models");

    let client = state.http_client.clone();
    let api_key = state.config.backend_api_key.as_deref();

    // Check both endpoints concurrently — return OK if either succeeds first.
    let health_fut = {
        let mut req = client.get(&health_url).timeout(Duration::from_secs(5));
        if let Some(key) = api_key {
            req = req.bearer_auth(key);
        }
        req.send()
    };
    let models_fut = {
        let mut req = client.get(&models_url).timeout(Duration::from_secs(5));
        if let Some(key) = api_key {
            req = req.bearer_auth(key);
        }
        req.send()
    };

    // Run both concurrently — first success wins, worst case is one 5s timeout
    // instead of two sequential 5s timeouts.
    let (health_result, models_result) = tokio::join!(health_fut, models_fut);

    let ok = health_result
        .map(|r| r.status().is_success())
        .unwrap_or(false)
        || models_result
            .map(|r| r.status().is_success())
            .unwrap_or(false);

    if ok {
        return (StatusCode::OK, Json(json!({"status": "ok"})));
    }

    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(json!({"status": "error", "message": "Backend unreachable"})),
    )
}
