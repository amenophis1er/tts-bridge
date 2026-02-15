pub mod config;
pub mod health;
pub mod proxy;

use std::sync::Arc;

use axum::Router;
use axum::routing::get;

use config::Config;

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub http_client: reqwest::Client,
}

pub fn build_router(state: AppState) -> Router {
    Router::new()
        .route("/health", get(health::health_check))
        .route("/v1/audio/stream", get(proxy::ws_handler))
        .with_state(state)
}
