use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use tokio::net::TcpListener;
use tokio::signal;
use tracing::{info, warn};

use tts_bridge::AppState;
use tts_bridge::config::{Config, LogFormat};

const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(10);

#[tokio::main]
async fn main() {
    let config = Config::from_env();

    match config.log_format {
        LogFormat::Json => {
            tracing_subscriber::fmt()
                .json()
                .with_env_filter(
                    tracing_subscriber::EnvFilter::try_new(&config.log_level)
                        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
                )
                .init();
        }
        LogFormat::Plain => {
            tracing_subscriber::fmt()
                .with_env_filter(
                    tracing_subscriber::EnvFilter::try_new(&config.log_level)
                        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
                )
                .init();
        }
    }

    let port = config.port;

    let http_client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .read_timeout(Duration::from_secs(300))
        .build()
        .expect("Failed to build HTTP client");

    let state = AppState {
        config: Arc::new(config),
        http_client,
    };

    let app = tts_bridge::build_router(state);

    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    let listener = TcpListener::bind(addr).await.expect("Failed to bind");

    info!(port = port, "tts-bridge listening");

    // Use a oneshot channel so we know when the signal fires, then enforce
    // a drain timeout: if connections don't close within SHUTDOWN_TIMEOUT,
    // the server future is dropped (force-closing them).
    let (drain_tx, drain_rx) = tokio::sync::oneshot::channel::<()>();

    let server = axum::serve(listener, app).with_graceful_shutdown(async {
        shutdown_signal().await;
        let _ = drain_tx.send(());
    });

    tokio::select! {
        result = server => {
            if let Err(e) = result {
                warn!("Server error: {e}");
            }
        }
        _ = async {
            // Wait until the shutdown signal fires, then start the drain timer
            let _ = drain_rx.await;
            info!("Draining connections for up to {SHUTDOWN_TIMEOUT:?}");
            tokio::time::sleep(SHUTDOWN_TIMEOUT).await;
            warn!("Shutdown timeout reached, force-closing connections");
        } => {}
    }
}

async fn shutdown_signal() {
    let ctrl_c = async {
        signal::ctrl_c()
            .await
            .expect("Failed to install Ctrl+C handler");
    };

    let terminate = async {
        signal::unix::signal(signal::unix::SignalKind::terminate())
            .expect("Failed to install SIGTERM handler")
            .recv()
            .await;
    };

    tokio::select! {
        () = ctrl_c => { info!("Received SIGINT, shutting down"); }
        () = terminate => { info!("Received SIGTERM, shutting down"); }
    }
}
