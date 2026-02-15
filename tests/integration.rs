use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::post;
use futures_util::sink::SinkExt;
use futures_util::stream::StreamExt;
use tokio::net::TcpListener;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;
use tts_bridge::AppState;
use tts_bridge::config::Config;

/// Start a mock TTS backend that returns chunked PCM bytes.
async fn start_mock_backend() -> (SocketAddr, tokio::task::JoinHandle<()>) {
    let app = Router::new()
        .route("/v1/audio/speech", post(mock_tts_handler))
        .route("/health", axum::routing::get(|| async { "ok" }));

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let handle = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    (addr, handle)
}

/// Mock TTS handler: returns 4 chunks of 100 bytes each with small delays.
async fn mock_tts_handler() -> impl IntoResponse {
    let stream = futures_util::stream::iter((0..4).map(|i| {
        let chunk = vec![(i + 1) as u8; 100];
        Ok::<_, std::io::Error>(chunk)
    }));

    let body = Body::from_stream(stream);
    (StatusCode::OK, body)
}

/// Start a mock backend that returns 503.
async fn start_error_backend() -> (SocketAddr, tokio::task::JoinHandle<()>) {
    let app = Router::new().route(
        "/v1/audio/speech",
        post(|| async { (StatusCode::SERVICE_UNAVAILABLE, "model not loaded") }),
    );

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let handle = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    (addr, handle)
}

/// Start a mock backend with slow streaming (for cancel tests).
async fn start_slow_backend() -> (SocketAddr, tokio::task::JoinHandle<()>) {
    let app = Router::new().route("/v1/audio/speech", post(slow_tts_handler));

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let handle = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    (addr, handle)
}

async fn slow_tts_handler() -> impl IntoResponse {
    let stream = futures_util::stream::unfold(0u8, |i| async move {
        if i >= 20 {
            return None;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
        let chunk = vec![i + 1; 100];
        Some((Ok::<_, std::io::Error>(chunk), i + 1))
    });

    let body = Body::from_stream(stream);
    (StatusCode::OK, body)
}

/// Build and start the tts-bridge server pointing at a given backend.
async fn start_bridge(backend_addr: SocketAddr) -> (SocketAddr, tokio::task::JoinHandle<()>) {
    start_bridge_with_config(backend_addr, 4800).await
}

async fn start_bridge_with_config(
    backend_addr: SocketAddr,
    chunk_size: usize,
) -> (SocketAddr, tokio::task::JoinHandle<()>) {
    let config = Config {
        backend_url: format!("http://{backend_addr}"),
        backend_api_key: None,
        default_model: "kokoro".into(),
        default_voice: "af_heart".into(),
        chunk_size,
        max_buffer_size: 5_242_880,
        port: 0,
        log_level: "debug".into(),
        log_format: tts_bridge::config::LogFormat::Plain,
    };

    let state = AppState {
        config: Arc::new(config),
        http_client: reqwest::Client::new(),
    };

    let app = tts_bridge::build_router(state);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let handle = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    (addr, handle)
}

/// Connect to the bridge WebSocket.
async fn ws_connect(
    addr: SocketAddr,
) -> (
    futures_util::stream::SplitSink<
        tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
        Message,
    >,
    futures_util::stream::SplitStream<
        tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
    >,
) {
    let url = format!("ws://{addr}/v1/audio/stream");
    let (ws, _) = connect_async(&url).await.expect("Failed to connect");
    ws.split()
}

/// Read next text message, with a timeout.
async fn recv_text(
    reader: &mut futures_util::stream::SplitStream<
        tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
    >,
) -> serde_json::Value {
    let msg = tokio::time::timeout(Duration::from_secs(5), reader.next())
        .await
        .expect("Timeout waiting for message")
        .expect("Stream ended")
        .expect("WS error");

    match msg {
        Message::Text(t) => serde_json::from_str(&t).expect("Invalid JSON"),
        other => panic!("Expected text message, got: {other:?}"),
    }
}

/// Read next binary message, with a timeout.
async fn recv_binary(
    reader: &mut futures_util::stream::SplitStream<
        tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
    >,
) -> Vec<u8> {
    let msg = tokio::time::timeout(Duration::from_secs(5), reader.next())
        .await
        .expect("Timeout waiting for message")
        .expect("Stream ended")
        .expect("WS error");

    match msg {
        Message::Binary(b) => b.to_vec(),
        other => panic!("Expected binary message, got: {other:?}"),
    }
}

/// Read next message of any type, with a timeout.
async fn recv_msg(
    reader: &mut futures_util::stream::SplitStream<
        tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
    >,
) -> Message {
    tokio::time::timeout(Duration::from_secs(5), reader.next())
        .await
        .expect("Timeout waiting for message")
        .expect("Stream ended")
        .expect("WS error")
}

// ============================================================
// Tests
// ============================================================

#[tokio::test]
async fn test_happy_path() {
    let (backend_addr, _bh) = start_mock_backend().await;
    // Use chunk_size=100 to match mock's 100-byte chunks for predictable framing
    let (bridge_addr, _sh) = start_bridge_with_config(backend_addr, 100).await;
    let (mut writer, mut reader) = ws_connect(bridge_addr).await;

    // Send synthesize request
    writer
        .send(Message::Text(
            serde_json::json!({"text": "Hello world"})
                .to_string()
                .into(),
        ))
        .await
        .unwrap();

    // Expect start frame
    let start = recv_text(&mut reader).await;
    assert_eq!(start["type"], "start");
    assert!(start["utterance_id"].is_string());
    assert_eq!(start["sample_rate"], 24000);
    assert_eq!(start["channels"], 1);

    // Expect binary chunks (mock sends 4 x 100 bytes, chunk_size=100 → 4 frames)
    let mut total_bytes = 0;
    for _ in 0..4 {
        let data = recv_binary(&mut reader).await;
        assert_eq!(data.len(), 100);
        total_bytes += data.len();
    }
    assert_eq!(total_bytes, 400);

    // Expect done frame
    let done = recv_text(&mut reader).await;
    assert_eq!(done["type"], "done");
    assert_eq!(done["utterance_id"], start["utterance_id"]);
}

#[tokio::test]
async fn test_sticky_params() {
    let (backend_addr, _bh) = start_mock_backend().await;
    let (bridge_addr, _sh) = start_bridge_with_config(backend_addr, 100).await;
    let (mut writer, mut reader) = ws_connect(bridge_addr).await;

    // First request with custom voice
    writer
        .send(Message::Text(
            serde_json::json!({"text": "First", "voice": "custom_voice"})
                .to_string()
                .into(),
        ))
        .await
        .unwrap();

    // Drain start + binary + done
    let start1 = recv_text(&mut reader).await;
    assert_eq!(start1["type"], "start");
    for _ in 0..4 {
        recv_binary(&mut reader).await;
    }
    let done1 = recv_text(&mut reader).await;
    assert_eq!(done1["type"], "done");

    // Second request without specifying voice (should use sticky "custom_voice")
    writer
        .send(Message::Text(
            serde_json::json!({"text": "Second"}).to_string().into(),
        ))
        .await
        .unwrap();

    let start2 = recv_text(&mut reader).await;
    assert_eq!(start2["type"], "start");
    // We can't directly verify the backend received "custom_voice" without a more
    // sophisticated mock, but the connection stays healthy and produces output.
    for _ in 0..4 {
        recv_binary(&mut reader).await;
    }
    let done2 = recv_text(&mut reader).await;
    assert_eq!(done2["type"], "done");
}

#[tokio::test]
async fn test_cancel() {
    let (backend_addr, _bh) = start_slow_backend().await;
    let (bridge_addr, _sh) = start_bridge_with_config(backend_addr, 100).await;
    let (mut writer, mut reader) = ws_connect(bridge_addr).await;

    // Send synthesize
    writer
        .send(Message::Text(
            serde_json::json!({"text": "Long text"}).to_string().into(),
        ))
        .await
        .unwrap();

    // Expect start
    let start = recv_text(&mut reader).await;
    assert_eq!(start["type"], "start");

    // Read at least one binary chunk
    recv_binary(&mut reader).await;

    // Send cancel
    writer
        .send(Message::Text(
            serde_json::json!({"type": "cancel"}).to_string().into(),
        ))
        .await
        .unwrap();

    // Drain any remaining binary frames until we get the cancelled text frame
    loop {
        let msg = recv_msg(&mut reader).await;
        match msg {
            Message::Binary(_) => continue,
            Message::Text(t) => {
                let v: serde_json::Value = serde_json::from_str(&t).unwrap();
                assert_eq!(v["type"], "cancelled");
                assert_eq!(v["utterance_id"], start["utterance_id"]);
                break;
            }
            other => panic!("Unexpected message: {other:?}"),
        }
    }

    // Connection should still be open — send another utterance
    writer
        .send(Message::Text(
            serde_json::json!({"text": "After cancel"})
                .to_string()
                .into(),
        ))
        .await
        .unwrap();

    let start2 = recv_text(&mut reader).await;
    assert_eq!(start2["type"], "start");
}

#[tokio::test]
async fn test_reset() {
    let (backend_addr, _bh) = start_mock_backend().await;
    let (bridge_addr, _sh) = start_bridge_with_config(backend_addr, 100).await;
    let (mut writer, mut reader) = ws_connect(bridge_addr).await;

    // Set a custom voice
    writer
        .send(Message::Text(
            serde_json::json!({"text": "With voice", "voice": "custom"})
                .to_string()
                .into(),
        ))
        .await
        .unwrap();

    // Drain the response
    recv_text(&mut reader).await; // start
    for _ in 0..4 {
        recv_binary(&mut reader).await;
    }
    recv_text(&mut reader).await; // done

    // Reset
    writer
        .send(Message::Text(
            serde_json::json!({"type": "reset"}).to_string().into(),
        ))
        .await
        .unwrap();

    // Send another utterance — should use defaults
    writer
        .send(Message::Text(
            serde_json::json!({"text": "After reset"})
                .to_string()
                .into(),
        ))
        .await
        .unwrap();

    let start = recv_text(&mut reader).await;
    assert_eq!(start["type"], "start");
    for _ in 0..4 {
        recv_binary(&mut reader).await;
    }
    let done = recv_text(&mut reader).await;
    assert_eq!(done["type"], "done");
}

#[tokio::test]
async fn test_malformed_json() {
    let (backend_addr, _bh) = start_mock_backend().await;
    let (bridge_addr, _sh) = start_bridge(backend_addr).await;
    let (mut writer, mut reader) = ws_connect(bridge_addr).await;

    // Send garbage
    writer
        .send(Message::Text("not json {{{".into()))
        .await
        .unwrap();

    let err = recv_text(&mut reader).await;
    assert_eq!(err["type"], "error");
    assert!(err["message"].as_str().unwrap().contains("Invalid JSON"));

    // Connection should still be open
    writer
        .send(Message::Text(
            serde_json::json!({"text": "After error"})
                .to_string()
                .into(),
        ))
        .await
        .unwrap();

    let start = recv_text(&mut reader).await;
    assert_eq!(start["type"], "start");
}

#[tokio::test]
async fn test_binary_frame_from_client() {
    let (backend_addr, _bh) = start_mock_backend().await;
    let (bridge_addr, _sh) = start_bridge(backend_addr).await;
    let (mut writer, mut reader) = ws_connect(bridge_addr).await;

    // Send binary frame
    writer
        .send(Message::Binary(vec![0, 1, 2, 3].into()))
        .await
        .unwrap();

    // Expect close with code 1003
    let msg = recv_msg(&mut reader).await;
    match msg {
        Message::Close(Some(frame)) => {
            assert_eq!(
                frame.code,
                tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode::Unsupported
            );
        }
        Message::Close(None) => {
            // Some implementations may not include the frame
        }
        other => panic!("Expected close message, got: {other:?}"),
    }
}

#[tokio::test]
async fn test_concurrent_utterance_rejected() {
    let (backend_addr, _bh) = start_slow_backend().await;
    let (bridge_addr, _sh) = start_bridge_with_config(backend_addr, 100).await;
    let (mut writer, mut reader) = ws_connect(bridge_addr).await;

    // Start first utterance
    writer
        .send(Message::Text(
            serde_json::json!({"text": "First long text"})
                .to_string()
                .into(),
        ))
        .await
        .unwrap();

    // Wait for start
    let start = recv_text(&mut reader).await;
    assert_eq!(start["type"], "start");

    // Read at least one audio chunk to ensure streaming is active
    recv_binary(&mut reader).await;

    // Try to send another utterance
    writer
        .send(Message::Text(
            serde_json::json!({"text": "Second text"})
                .to_string()
                .into(),
        ))
        .await
        .unwrap();

    // Should get an error about utterance in progress
    // Drain binary chunks until we get a text message
    loop {
        let msg = recv_msg(&mut reader).await;
        match msg {
            Message::Binary(_) => continue,
            Message::Text(t) => {
                let v: serde_json::Value = serde_json::from_str(&t).unwrap();
                if v["type"] == "error" {
                    assert!(v["message"].as_str().unwrap().contains("still in progress"));
                    break;
                }
            }
            other => panic!("Unexpected message: {other:?}"),
        }
    }
}

#[tokio::test]
async fn test_backend_error() {
    let (backend_addr, _bh) = start_error_backend().await;
    let (bridge_addr, _sh) = start_bridge(backend_addr).await;
    let (mut writer, mut reader) = ws_connect(bridge_addr).await;

    writer
        .send(Message::Text(
            serde_json::json!({"text": "Hello"}).to_string().into(),
        ))
        .await
        .unwrap();

    // C2 fix: No start frame before backend error — error frame comes directly
    let err = recv_text(&mut reader).await;
    assert_eq!(err["type"], "error");
    assert!(err["utterance_id"].is_string());
    assert!(err["message"].as_str().unwrap().contains("503"));

    // Connection should still be open
    writer
        .send(Message::Text(
            serde_json::json!({"text": "Retry"}).to_string().into(),
        ))
        .await
        .unwrap();

    // This time backend would also fail, but we just check the connection is alive
    let msg = recv_text(&mut reader).await;
    assert!(msg["type"] == "error" || msg["type"] == "start");
}

#[tokio::test]
async fn test_empty_text() {
    let (backend_addr, _bh) = start_mock_backend().await;
    let (bridge_addr, _sh) = start_bridge_with_config(backend_addr, 100).await;
    let (mut writer, mut reader) = ws_connect(bridge_addr).await;

    // Send empty text — should forward to backend
    writer
        .send(Message::Text(
            serde_json::json!({"text": ""}).to_string().into(),
        ))
        .await
        .unwrap();

    let start = recv_text(&mut reader).await;
    assert_eq!(start["type"], "start");
}

#[tokio::test]
async fn test_custom_utterance_id() {
    let (backend_addr, _bh) = start_mock_backend().await;
    let (bridge_addr, _sh) = start_bridge_with_config(backend_addr, 100).await;
    let (mut writer, mut reader) = ws_connect(bridge_addr).await;

    writer
        .send(Message::Text(
            serde_json::json!({"text": "Hello", "utterance_id": "my-custom-id"})
                .to_string()
                .into(),
        ))
        .await
        .unwrap();

    let start = recv_text(&mut reader).await;
    assert_eq!(start["type"], "start");
    assert_eq!(start["utterance_id"], "my-custom-id");

    for _ in 0..4 {
        recv_binary(&mut reader).await;
    }

    let done = recv_text(&mut reader).await;
    assert_eq!(done["type"], "done");
    assert_eq!(done["utterance_id"], "my-custom-id");
}

#[tokio::test]
async fn test_missing_text_field() {
    let (backend_addr, _bh) = start_mock_backend().await;
    let (bridge_addr, _sh) = start_bridge(backend_addr).await;
    let (mut writer, mut reader) = ws_connect(bridge_addr).await;

    // Valid JSON but missing "text" field and no "type" for control
    writer
        .send(Message::Text(
            serde_json::json!({"voice": "something"}).to_string().into(),
        ))
        .await
        .unwrap();

    let err = recv_text(&mut reader).await;
    assert_eq!(err["type"], "error");
    assert!(err["message"].as_str().unwrap().contains("text"));
}

#[tokio::test]
async fn test_unknown_message_type() {
    let (backend_addr, _bh) = start_mock_backend().await;
    let (bridge_addr, _sh) = start_bridge(backend_addr).await;
    let (mut writer, mut reader) = ws_connect(bridge_addr).await;

    // Send message with unknown type
    writer
        .send(Message::Text(
            serde_json::json!({"type": "pause"}).to_string().into(),
        ))
        .await
        .unwrap();

    let err = recv_text(&mut reader).await;
    assert_eq!(err["type"], "error");
    assert!(
        err["message"]
            .as_str()
            .unwrap()
            .contains("Unknown message type")
    );

    // Connection should still be open
    writer
        .send(Message::Text(
            serde_json::json!({"text": "Still works"})
                .to_string()
                .into(),
        ))
        .await
        .unwrap();

    let start = recv_text(&mut reader).await;
    assert_eq!(start["type"], "start");
}

#[tokio::test]
async fn test_extra_fields_forwarded() {
    // Use a mock backend that captures and echoes back the request body
    use std::sync::Mutex;

    let captured: Arc<Mutex<Option<serde_json::Value>>> = Arc::new(Mutex::new(None));
    let captured_clone = captured.clone();

    let app = Router::new().route(
        "/v1/audio/speech",
        post(move |body: axum::body::Bytes| {
            let captured = captured_clone.clone();
            async move {
                let parsed: serde_json::Value = serde_json::from_slice(&body).unwrap();
                *captured.lock().unwrap() = Some(parsed);
                let stream =
                    futures_util::stream::iter(vec![Ok::<_, std::io::Error>(vec![0u8; 100])]);
                (StatusCode::OK, Body::from_stream(stream))
            }
        }),
    );

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let backend_addr = listener.local_addr().unwrap();
    let _bh = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    let (bridge_addr, _sh) = start_bridge_with_config(backend_addr, 100).await;
    let (mut writer, mut reader) = ws_connect(bridge_addr).await;

    // Send with extra backend-specific fields
    writer
        .send(Message::Text(
            serde_json::json!({
                "text": "Hello",
                "style": "cheerful",
                "emotion": "happy"
            })
            .to_string()
            .into(),
        ))
        .await
        .unwrap();

    let start = recv_text(&mut reader).await;
    assert_eq!(start["type"], "start");

    // Drain audio + done
    recv_binary(&mut reader).await;
    recv_text(&mut reader).await;

    // Verify the backend received the extra fields
    let body = captured.lock().unwrap().take().unwrap();
    assert_eq!(body["input"], "Hello");
    assert_eq!(body["style"], "cheerful");
    assert_eq!(body["emotion"], "happy");
    // Known fields should also be present
    assert_eq!(body["model"], "kokoro");
    assert_eq!(body["voice"], "af_heart");
    assert_eq!(body["response_format"], "pcm");
}

#[tokio::test]
async fn test_buffer_overflow_closes_connection() {
    // Mock backend that streams a large amount of data
    let app = Router::new().route(
        "/v1/audio/speech",
        post(|| async {
            // Stream 200 chunks of 1000 bytes = 200KB total
            let stream = futures_util::stream::iter(
                (0..200).map(|_| Ok::<_, std::io::Error>(vec![0u8; 1000])),
            );
            (StatusCode::OK, Body::from_stream(stream))
        }),
    );

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let backend_addr = listener.local_addr().unwrap();
    let _bh = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    // Use a very small max_buffer_size to trigger overflow
    let config = Config {
        backend_url: format!("http://{backend_addr}"),
        backend_api_key: None,
        default_model: "kokoro".into(),
        default_voice: "af_heart".into(),
        chunk_size: 100,
        max_buffer_size: 500, // Very small — will overflow quickly
        port: 0,
        log_level: "debug".into(),
        log_format: tts_bridge::config::LogFormat::Plain,
    };

    let state = AppState {
        config: Arc::new(config),
        http_client: reqwest::Client::new(),
    };

    let app = tts_bridge::build_router(state);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let bridge_addr = listener.local_addr().unwrap();
    let _sh = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    let (mut writer, mut reader) = ws_connect(bridge_addr).await;

    writer
        .send(Message::Text(
            serde_json::json!({"text": "overflow"}).to_string().into(),
        ))
        .await
        .unwrap();

    // Should get start, then some binary, then error, then close
    let mut got_error = false;
    let mut got_close = false;
    loop {
        match tokio::time::timeout(Duration::from_secs(5), reader.next()).await {
            Ok(Some(Ok(Message::Text(t)))) => {
                let v: serde_json::Value = serde_json::from_str(&t).unwrap();
                if v["type"] == "error" && v["message"].as_str().unwrap().contains("Buffer size") {
                    got_error = true;
                }
            }
            Ok(Some(Ok(Message::Binary(_)))) => continue,
            Ok(Some(Ok(Message::Close(_)))) => {
                got_close = true;
                break;
            }
            Ok(None) => {
                // Stream ended (connection closed)
                got_close = true;
                break;
            }
            _ => break,
        }
    }

    assert!(
        got_error,
        "Should have received a buffer overflow error frame"
    );
    assert!(
        got_close,
        "Connection should have been closed after overflow"
    );
}

#[tokio::test]
async fn test_reset_clears_extras() {
    use std::sync::Mutex;

    let captured: Arc<Mutex<Vec<serde_json::Value>>> = Arc::new(Mutex::new(Vec::new()));
    let captured_clone = captured.clone();

    let app = Router::new().route(
        "/v1/audio/speech",
        post(move |body: axum::body::Bytes| {
            let captured = captured_clone.clone();
            async move {
                let parsed: serde_json::Value = serde_json::from_slice(&body).unwrap();
                captured.lock().unwrap().push(parsed);
                let stream =
                    futures_util::stream::iter(vec![Ok::<_, std::io::Error>(vec![0u8; 100])]);
                (StatusCode::OK, Body::from_stream(stream))
            }
        }),
    );

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let backend_addr = listener.local_addr().unwrap();
    let _bh = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    let (bridge_addr, _sh) = start_bridge_with_config(backend_addr, 100).await;
    let (mut writer, mut reader) = ws_connect(bridge_addr).await;

    // Send with extra field
    writer
        .send(Message::Text(
            serde_json::json!({"text": "one", "style": "cheerful"})
                .to_string()
                .into(),
        ))
        .await
        .unwrap();
    recv_text(&mut reader).await; // start
    recv_binary(&mut reader).await;
    recv_text(&mut reader).await; // done

    // Reset
    writer
        .send(Message::Text(
            serde_json::json!({"type": "reset"}).to_string().into(),
        ))
        .await
        .unwrap();

    // Send without extras — should NOT have "style" anymore
    writer
        .send(Message::Text(
            serde_json::json!({"text": "two"}).to_string().into(),
        ))
        .await
        .unwrap();
    recv_text(&mut reader).await; // start
    recv_binary(&mut reader).await;
    recv_text(&mut reader).await; // done

    let bodies = captured.lock().unwrap();
    // First request should have "style"
    assert_eq!(bodies[0]["style"], "cheerful");
    // Second request (after reset) should NOT have "style"
    assert!(bodies[1].get("style").is_none());
}

#[tokio::test]
async fn test_extras_sticky_across_utterances() {
    use std::sync::Mutex;

    let captured: Arc<Mutex<Vec<serde_json::Value>>> = Arc::new(Mutex::new(Vec::new()));
    let captured_clone = captured.clone();

    let app = Router::new().route(
        "/v1/audio/speech",
        post(move |body: axum::body::Bytes| {
            let captured = captured_clone.clone();
            async move {
                let parsed: serde_json::Value = serde_json::from_slice(&body).unwrap();
                captured.lock().unwrap().push(parsed);
                let stream =
                    futures_util::stream::iter(vec![Ok::<_, std::io::Error>(vec![0u8; 100])]);
                (StatusCode::OK, Body::from_stream(stream))
            }
        }),
    );

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let backend_addr = listener.local_addr().unwrap();
    let _bh = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    let (bridge_addr, _sh) = start_bridge_with_config(backend_addr, 100).await;
    let (mut writer, mut reader) = ws_connect(bridge_addr).await;

    // First utterance with extra field
    writer
        .send(Message::Text(
            serde_json::json!({"text": "one", "style": "cheerful"})
                .to_string()
                .into(),
        ))
        .await
        .unwrap();
    recv_text(&mut reader).await; // start
    recv_binary(&mut reader).await;
    recv_text(&mut reader).await; // done

    // Second utterance WITHOUT specifying style — should still have it (sticky)
    writer
        .send(Message::Text(
            serde_json::json!({"text": "two"}).to_string().into(),
        ))
        .await
        .unwrap();
    recv_text(&mut reader).await; // start
    recv_binary(&mut reader).await;
    recv_text(&mut reader).await; // done

    let bodies = captured.lock().unwrap();
    assert_eq!(bodies[0]["style"], "cheerful");
    assert_eq!(
        bodies[1]["style"], "cheerful",
        "extras should persist across utterances"
    );
}

#[tokio::test]
async fn test_cancel_when_idle() {
    let (backend_addr, _bh) = start_mock_backend().await;
    let (bridge_addr, _sh) = start_bridge(backend_addr).await;
    let (mut writer, mut reader) = ws_connect(bridge_addr).await;

    // Send cancel with no active utterance
    writer
        .send(Message::Text(
            serde_json::json!({"type": "cancel"}).to_string().into(),
        ))
        .await
        .unwrap();

    // Connection should still be open — send a normal utterance
    writer
        .send(Message::Text(
            serde_json::json!({"text": "After idle cancel"})
                .to_string()
                .into(),
        ))
        .await
        .unwrap();

    let start = recv_text(&mut reader).await;
    assert_eq!(start["type"], "start");
}

#[tokio::test]
async fn test_pcm16_padding_odd_final_buffer() {
    // Mock backend that returns an odd number of bytes total
    let app = Router::new().route(
        "/v1/audio/speech",
        post(|| async {
            // 7 bytes — odd, needs padding to 8
            let stream = futures_util::stream::iter(vec![Ok::<_, std::io::Error>(vec![0xABu8; 7])]);
            (StatusCode::OK, Body::from_stream(stream))
        }),
    );

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let backend_addr = listener.local_addr().unwrap();
    let _bh = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    // chunk_size larger than 7, so all bytes end up in the final flush
    let (bridge_addr, _sh) = start_bridge_with_config(backend_addr, 100).await;
    let (mut writer, mut reader) = ws_connect(bridge_addr).await;

    writer
        .send(Message::Text(
            serde_json::json!({"text": "odd"}).to_string().into(),
        ))
        .await
        .unwrap();

    let start = recv_text(&mut reader).await;
    assert_eq!(start["type"], "start");

    let data = recv_binary(&mut reader).await;
    // Should be padded to 8 bytes (even) for PCM16 alignment
    assert_eq!(data.len(), 8);
    assert_eq!(data[6], 0xAB); // last real byte
    assert_eq!(data[7], 0x00); // padding byte

    let done = recv_text(&mut reader).await;
    assert_eq!(done["type"], "done");
}

#[tokio::test]
async fn test_extras_cannot_override_response_format() {
    use std::sync::Mutex;

    let captured: Arc<Mutex<Option<serde_json::Value>>> = Arc::new(Mutex::new(None));
    let captured_clone = captured.clone();

    let app = Router::new().route(
        "/v1/audio/speech",
        post(move |body: axum::body::Bytes| {
            let captured = captured_clone.clone();
            async move {
                let parsed: serde_json::Value = serde_json::from_slice(&body).unwrap();
                *captured.lock().unwrap() = Some(parsed);
                let stream =
                    futures_util::stream::iter(vec![Ok::<_, std::io::Error>(vec![0u8; 100])]);
                (StatusCode::OK, Body::from_stream(stream))
            }
        }),
    );

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let backend_addr = listener.local_addr().unwrap();
    let _bh = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    let (bridge_addr, _sh) = start_bridge_with_config(backend_addr, 100).await;
    let (mut writer, mut reader) = ws_connect(bridge_addr).await;

    // Try to override response_format via extras
    writer
        .send(Message::Text(
            serde_json::json!({"text": "Hello", "response_format": "mp3", "input": "injected"})
                .to_string()
                .into(),
        ))
        .await
        .unwrap();

    recv_text(&mut reader).await; // start
    recv_binary(&mut reader).await;
    recv_text(&mut reader).await; // done

    let body = captured.lock().unwrap().take().unwrap();
    // Known fields must NOT be overridden by client extras
    assert_eq!(
        body["response_format"], "pcm",
        "response_format must always be pcm"
    );
    assert_eq!(body["input"], "Hello", "input must match the text field");
}
