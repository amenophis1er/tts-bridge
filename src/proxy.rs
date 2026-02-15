use axum::extract::State;
use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::response::IntoResponse;
use futures_util::sink::SinkExt;
use futures_util::stream::StreamExt;
use serde_json::json;
use tracing::{debug, error, info, warn};

use crate::AppState;

/// Known fields that are extracted from client messages for control/sticky logic.
/// Everything else is forwarded verbatim to the backend.
const CONTROL_FIELDS: &[&str] = &["text", "type", "utterance_id"];
const STICKY_FIELDS: &[&str] = &["voice", "model", "sample_rate", "language", "speed"];

#[derive(Debug)]
struct SessionParams {
    voice: String,
    model: String,
    sample_rate: u32,
    language: String,
    speed: f64,
    /// Extra client-provided fields carried across utterances (sticky).
    extras: serde_json::Map<String, serde_json::Value>,
}

impl SessionParams {
    fn new(config: &crate::config::Config) -> Self {
        SessionParams {
            voice: config.default_voice.clone(),
            model: config.default_model.clone(),
            sample_rate: 24000,
            language: "en".into(),
            speed: 1.0,
            extras: serde_json::Map::new(),
        }
    }
}

enum ClientMessage {
    Synthesize {
        text: String,
        utterance_id: Option<String>,
        voice: Option<String>,
        model: Option<String>,
        sample_rate: Option<u32>,
        language: Option<String>,
        speed: Option<f64>,
        /// All fields that are not control or known sticky fields.
        extra_fields: serde_json::Map<String, serde_json::Value>,
    },
    Cancel,
    Reset,
}

enum StreamResult {
    Done,
    Cancelled,
    ClientDisconnected,
    /// Fatal error — connection should be closed after sending the error frame.
    Fatal(String),
}

fn parse_client_message(text: &str) -> Result<ClientMessage, String> {
    let value: serde_json::Value =
        serde_json::from_str(text).map_err(|e| format!("Invalid JSON: {e}"))?;

    let obj = value
        .as_object()
        .ok_or_else(|| "Expected JSON object".to_string())?;

    // Check for control messages
    if let Some(msg_type) = obj.get("type").and_then(|v| v.as_str()) {
        match msg_type {
            "cancel" => return Ok(ClientMessage::Cancel),
            "reset" => return Ok(ClientMessage::Reset),
            other => return Err(format!("Unknown message type: \"{other}\"")),
        }
    }

    let text = obj
        .get("text")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| "Missing required field: \"text\"".to_string())?;

    let utterance_id = obj
        .get("utterance_id")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    let voice = obj
        .get("voice")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let model = obj
        .get("model")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let sample_rate = obj
        .get("sample_rate")
        .and_then(|v| v.as_u64())
        .map(|n| n as u32);
    let language = obj
        .get("language")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let speed = obj.get("speed").and_then(|v| v.as_f64());

    // Collect extra fields (anything not in CONTROL_FIELDS or STICKY_FIELDS)
    let extra_fields: serde_json::Map<String, serde_json::Value> = obj
        .iter()
        .filter(|(k, _)| {
            !CONTROL_FIELDS.contains(&k.as_str()) && !STICKY_FIELDS.contains(&k.as_str())
        })
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();

    Ok(ClientMessage::Synthesize {
        text,
        utterance_id,
        voice,
        model,
        sample_rate,
        language,
        speed,
        extra_fields,
    })
}

pub async fn ws_handler(ws: WebSocketUpgrade, State(state): State<AppState>) -> impl IntoResponse {
    ws.max_message_size(1024 * 1024) // 1MB limit on incoming messages
        .on_upgrade(|socket| handle_connection(socket, state))
}

async fn handle_connection(socket: WebSocket, state: AppState) {
    let (mut ws_sender, mut ws_receiver) = socket.split();
    let mut session = SessionParams::new(&state.config);
    let session_id = uuid::Uuid::new_v4();

    info!(session_id = %session_id, "WebSocket connection established");

    loop {
        let msg = match ws_receiver.next().await {
            Some(Ok(Message::Text(text))) => text,
            Some(Ok(Message::Binary(_))) => {
                warn!(session_id = %session_id, "Client sent binary frame, closing with 1003");
                let _ = ws_sender
                    .send(Message::Close(Some(CloseFrame {
                        code: 1003,
                        reason: "Binary frames not supported".into(),
                    })))
                    .await;
                break;
            }
            Some(Ok(Message::Close(_))) | None => {
                info!(session_id = %session_id, "Client disconnected");
                break;
            }
            Some(Ok(Message::Ping(data))) => {
                let _ = ws_sender.send(Message::Pong(data)).await;
                continue;
            }
            Some(Ok(Message::Pong(_))) => continue,
            Some(Err(e)) => {
                error!(session_id = %session_id, error = %e, "WebSocket error");
                break;
            }
        };

        let client_msg = match parse_client_message(&msg) {
            Ok(m) => m,
            Err(e) => {
                let _ = send_error(&mut ws_sender, &e).await;
                continue;
            }
        };

        match client_msg {
            ClientMessage::Synthesize {
                text,
                utterance_id,
                voice,
                model,
                sample_rate,
                language,
                speed,
                extra_fields,
            } => {
                // Update sticky params
                if let Some(v) = voice {
                    session.voice = v;
                }
                if let Some(m) = model {
                    session.model = m;
                }
                if let Some(sr) = sample_rate {
                    session.sample_rate = sr;
                }
                if let Some(l) = language {
                    session.language = l;
                }
                if let Some(s) = speed {
                    session.speed = s;
                }
                // Merge extra fields into sticky extras
                for (k, v) in extra_fields {
                    session.extras.insert(k, v);
                }

                let uid = utterance_id.unwrap_or_else(|| format!("u_{}", uuid::Uuid::new_v4()));

                debug!(
                    session_id = %session_id,
                    utterance_id = %uid,
                    model = %session.model,
                    voice = %session.voice,
                    "Starting utterance"
                );

                // Stream audio (start frame is sent inside after backend responds)
                match stream_utterance(
                    &mut ws_sender,
                    &mut ws_receiver,
                    &state,
                    &session,
                    &text,
                    &uid,
                )
                .await
                {
                    Ok(StreamResult::Done) => {
                        let done_frame = json!({
                            "type": "done",
                            "utterance_id": uid,
                        });
                        if ws_sender
                            .send(Message::Text(done_frame.to_string().into()))
                            .await
                            .is_err()
                        {
                            break;
                        }
                        info!(
                            session_id = %session_id,
                            utterance_id = %uid,
                            "Utterance complete"
                        );
                    }
                    Ok(StreamResult::Cancelled) => {
                        let cancelled_frame = json!({
                            "type": "cancelled",
                            "utterance_id": uid,
                        });
                        let _ = ws_sender
                            .send(Message::Text(cancelled_frame.to_string().into()))
                            .await;
                        info!(
                            session_id = %session_id,
                            utterance_id = %uid,
                            "Utterance cancelled"
                        );
                    }
                    Ok(StreamResult::ClientDisconnected) => {
                        info!(session_id = %session_id, "Client disconnected during stream");
                        break;
                    }
                    Ok(StreamResult::Fatal(e)) => {
                        let error_frame = json!({
                            "type": "error",
                            "utterance_id": uid,
                            "message": e,
                        });
                        let _ = ws_sender
                            .send(Message::Text(error_frame.to_string().into()))
                            .await;
                        let _ = ws_sender
                            .send(Message::Close(Some(CloseFrame {
                                code: 1011, // Internal Error
                                reason: "Buffer limit exceeded".into(),
                            })))
                            .await;
                        error!(
                            session_id = %session_id,
                            utterance_id = %uid,
                            error = %e,
                            "Fatal stream error, closing connection"
                        );
                        break;
                    }
                    Err(e) => {
                        let error_frame = json!({
                            "type": "error",
                            "utterance_id": uid,
                            "message": e,
                        });
                        let _ = ws_sender
                            .send(Message::Text(error_frame.to_string().into()))
                            .await;
                        error!(
                            session_id = %session_id,
                            utterance_id = %uid,
                            error = %e,
                            "Utterance failed"
                        );
                    }
                }
            }
            ClientMessage::Cancel => {
                debug!(session_id = %session_id, "Cancel received outside of active utterance, ignoring");
            }
            ClientMessage::Reset => {
                session = SessionParams::new(&state.config);
                info!(session_id = %session_id, "Session parameters reset to defaults");
            }
        }
    }

    info!(session_id = %session_id, "WebSocket connection closed");
}

async fn stream_utterance(
    ws_sender: &mut futures_util::stream::SplitSink<WebSocket, Message>,
    ws_receiver: &mut futures_util::stream::SplitStream<WebSocket>,
    state: &AppState,
    params: &SessionParams,
    text: &str,
    utterance_id: &str,
) -> Result<StreamResult, String> {
    let url = format!("{}/v1/audio/speech", state.config.backend_url);

    // Build request body: extras first, then known fields override to prevent
    // clients from overriding critical fields like response_format or input.
    let mut body = serde_json::Map::new();

    // Forward arbitrary extra fields from sticky session first
    for (k, v) in &params.extras {
        body.insert(k.clone(), v.clone());
    }

    // Known fields always win — inserted after extras
    body.insert("model".into(), json!(params.model));
    body.insert("voice".into(), json!(params.voice));
    body.insert("input".into(), json!(text));
    body.insert("response_format".into(), json!("pcm"));
    body.insert("speed".into(), json!(params.speed));
    body.insert("sample_rate".into(), json!(params.sample_rate));
    body.insert("language".into(), json!(params.language));

    let mut request = state
        .http_client
        .post(&url)
        .json(&serde_json::Value::Object(body));

    if let Some(ref api_key) = state.config.backend_api_key {
        request = request.bearer_auth(api_key);
    }

    let response = request
        .send()
        .await
        .map_err(|e| format!("Backend request failed: {e}"))?;

    if !response.status().is_success() {
        let status = response.status();
        let body_text = response
            .text()
            .await
            .unwrap_or_else(|_| "unable to read response body".into());
        // Truncate long backend error responses (char-boundary safe)
        let truncated = if body_text.len() > 512 {
            let end = body_text
                .char_indices()
                .map(|(i, _)| i)
                .take_while(|&i| i <= 512)
                .last()
                .unwrap_or(0);
            format!("{}...", &body_text[..end])
        } else {
            body_text
        };
        return Err(format!("Backend returned {status}: {truncated}"));
    }

    // C2 fix: Send start frame only after successful backend response
    let start_frame = json!({
        "type": "start",
        "utterance_id": utterance_id,
        "sample_rate": params.sample_rate,
        "channels": 1,
    });
    if ws_sender
        .send(Message::Text(start_frame.to_string().into()))
        .await
        .is_err()
    {
        return Ok(StreamResult::ClientDisconnected);
    }

    let mut stream = response.bytes_stream();
    let chunk_size = state.config.chunk_size;
    let max_buffer_size = state.config.max_buffer_size;
    let mut buffer = Vec::with_capacity(chunk_size);

    loop {
        tokio::select! {
            // Bias toward client messages so cancel is processed immediately,
            // even when the backend is streaming data faster than we drain it.
            biased;

            ws_msg = ws_receiver.next() => {
                match ws_msg {
                    Some(Ok(Message::Text(incoming))) => {
                        if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&incoming) {
                            if parsed.get("type").and_then(|v| v.as_str()) == Some("cancel") {
                                debug!(utterance_id = %utterance_id, "Cancel received, aborting stream");
                                // Drop stream by returning — this aborts the HTTP request
                                return Ok(StreamResult::Cancelled);
                            }
                            if parsed.get("text").is_some() {
                                let err_msg = format!(
                                    "Utterance {utterance_id} is still in progress. Send {{\"type\":\"cancel\"}} first."
                                );
                                let error_frame = json!({
                                    "type": "error",
                                    "message": err_msg,
                                });
                                let _ = ws_sender
                                    .send(Message::Text(error_frame.to_string().into()))
                                    .await;
                            }
                        }
                    }
                    Some(Ok(Message::Binary(_))) => {
                        let _ = ws_sender
                            .send(Message::Close(Some(CloseFrame {
                                code: 1003,
                                reason: "Binary frames not supported".into(),
                            })))
                            .await;
                        return Ok(StreamResult::ClientDisconnected);
                    }
                    Some(Ok(Message::Close(_))) | None => {
                        return Ok(StreamResult::ClientDisconnected);
                    }
                    Some(Ok(Message::Ping(data))) => {
                        let _ = ws_sender.send(Message::Pong(data)).await;
                    }
                    _ => {}
                }
            }
            chunk = stream.next() => {
                match chunk {
                    Some(Ok(bytes)) => {
                        buffer.extend_from_slice(&bytes);

                        if buffer.len() > max_buffer_size {
                            return Ok(StreamResult::Fatal(format!(
                                "Buffer size exceeded maximum of {max_buffer_size} bytes"
                            )));
                        }

                        while buffer.len() >= chunk_size {
                            let frame: Vec<u8> =
                                buffer.drain(..chunk_size).collect();
                            if ws_sender
                                .send(Message::Binary(frame.into()))
                                .await
                                .is_err()
                            {
                                return Ok(StreamResult::ClientDisconnected);
                            }
                        }
                    }
                    Some(Err(e)) => {
                        return Err(format!("Backend stream error: {e}"));
                    }
                    None => {
                        // Stream complete — flush remaining bytes
                        if !buffer.is_empty() {
                            if buffer.len() % 2 != 0 {
                                buffer.push(0);
                            }
                            if ws_sender
                                .send(Message::Binary(buffer.into()))
                                .await
                                .is_err()
                            {
                                return Ok(StreamResult::ClientDisconnected);
                            }
                        }
                        return Ok(StreamResult::Done);
                    }
                }
            }
        }
    }
}

async fn send_error(
    ws_sender: &mut futures_util::stream::SplitSink<WebSocket, Message>,
    message: &str,
) -> Result<(), axum::Error> {
    let frame = json!({
        "type": "error",
        "message": message,
    });
    ws_sender
        .send(Message::Text(frame.to_string().into()))
        .await
}
