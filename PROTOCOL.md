# tts-bridge — WebSocket streaming for any OpenAI-compatible TTS API

## Problem

OpenAI-compatible TTS APIs (Speaches, OpenAI, Azure, local Kokoro) expose a single REST endpoint: `POST /v1/audio/speech`. You send text, you get back an audio file. That works for offline generation, but **real-time voice applications need streaming** — receiving audio chunks as they're generated, not waiting for the entire file.

Some backends support chunked transfer encoding on the response, but there's no standard way to:
- Stream multiple utterances over a persistent connection
- Receive audio as discrete, timed WebSocket frames ready for playback
- Know when an utterance is done vs when the next one starts
- Cancel a mid-flight utterance (barge-in)

## Solution

A lightweight Docker image that adds WebSocket streaming on top of any OpenAI-compatible TTS backend:

```
Voice App ──WS──► tts-bridge ──HTTP (streaming)──► TTS Backend
                  (this project)                    (Speaches, OpenAI, etc.)
```

Send text over WebSocket, receive PCM16 audio chunks in real-time. Persistent connection — multiple utterances on a single WebSocket. Zero inference, zero GPU — just protocol translation.

## Protocol

### Endpoint: `ws://host:port/v1/audio/stream`

#### 1. Connect

```
Client ──── WS connect ──────► Bridge
Client ◄─── WS accept ─────── Bridge
```

#### 2. Send text, receive audio

```json
// Client sends:
{
  "text": "Hello, how are you?",
  "voice": "af_heart",
  "model": "kokoro",
  "sample_rate": 24000,
  "language": "en"
}
```

```
Bridge ── POST /v1/audio/speech ──► Backend (stream response body)

Client ◄── {"type":"start","utterance_id":"u_a1b2","sample_rate":24000,"channels":1} ── Bridge
Client ◄── [binary: PCM16 chunk] ── Bridge ◄── audio bytes ── Backend
Client ◄── [binary: PCM16 chunk] ── Bridge ◄── audio bytes ── Backend
Client ◄── [binary: PCM16 chunk] ── Bridge ◄── EOF ────────── Backend
Client ◄── {"type":"done","utterance_id":"u_a1b2"}  ── Bridge
```

All client-provided parameters (`voice`, `model`, `sample_rate`, `language`, `speed`) are forwarded as-is to the backend `POST /v1/audio/speech` request body. Backends that don't recognize extra fields will ignore them; backends that support them (e.g., Speaches accepts `sample_rate` and `language`) will use them. The bridge is backend-agnostic — it never filters or validates backend-specific fields.

#### 3. Send another utterance (same connection)

```json
// Client sends:
{ "text": "I can help you with that." }
```

Parameters from the first message are **sticky** — `voice`, `model`, `sample_rate`, `language`, `speed`, and any extra backend-specific fields carry over. Override any of them per-message if needed.

#### 4. Cancel (barge-in)

If the client needs to interrupt a mid-flight utterance (e.g., user started speaking):

```json
// Client sends while audio is streaming:
{ "type": "cancel" }
```

The bridge immediately aborts the backend HTTP request and sends:

```json
{ "type": "cancelled", "utterance_id": "u_a1b2" }
```

No more audio frames are sent for that utterance. The connection stays open for the next utterance.

#### 5. Reset sticky parameters

```json
// Client sends:
{ "type": "reset" }
```

Clears all sticky parameters back to server defaults. Useful when switching voices or models mid-conversation.

#### 6. Sequential utterances only

One utterance streams at a time per WebSocket connection. If the client sends a new `text` message while an utterance is still in progress, the bridge responds with an error:

```json
{
  "type": "error",
  "message": "Utterance u_a1b2 is still in progress. Send {\"type\":\"cancel\"} first."
}
```

The in-flight utterance continues unaffected. For concurrent streams, open multiple WebSocket connections.

#### 7. Error handling

**Backend errors** — if the backend request fails, the bridge sends an error frame:

```json
{
  "type": "error",
  "utterance_id": "u_a1b2",
  "message": "Backend returned 503: model not loaded"
}
```

The WebSocket stays open — the client can retry with the next message.

**Malformed client messages** — if the client sends invalid JSON or a text frame missing the required `text` field (and no `type` field for control messages), the bridge sends an error frame without an `utterance_id`:

```json
{ "type": "error", "message": "Invalid JSON: expected '\"' at line 1 column 5" }
```

The connection stays open. A parse error is recoverable — the client can send a corrected message.

**Binary frames from client** — if the client sends a binary WebSocket frame (instead of a JSON text frame), the bridge closes the connection with WebSocket close code `1003` (unsupported data). Binary frames from the client indicate a fundamentally confused client, not a recoverable error.

### Message Reference

**Client → Bridge (JSON text frames):**

| Field | Type | Required | Default | Description |
|-------|------|----------|---------|-------------|
| `text` | string | yes* | — | Text to synthesize |
| `voice` | string | no | `af_heart` | Voice ID (backend-specific) |
| `model` | string | no | `kokoro` | TTS model name |
| `sample_rate` | number | no | `24000` | Output sample rate in Hz |
| `language` | string | no | `en` | Language code (e.g., `en`, `fr`, `ja`) |
| `speed` | number | no | `1.0` | Playback speed multiplier |
| `utterance_id` | string | no | auto-generated | Client-provided ID for correlation |

*Not required for control messages (`cancel`, `reset`).

All fields except `text`, `type`, and `utterance_id` are forwarded to the backend request body. This makes the bridge forward-compatible with any backend-specific parameters.

**Control messages (Client → Bridge):**

| Message | Description |
|---------|-------------|
| `{"type":"cancel"}` | Abort current utterance, stop sending audio |
| `{"type":"reset"}` | Clear sticky parameters back to defaults |

**Bridge → Client:**

| Frame | Type | Description |
|-------|------|-------------|
| `{"type":"start","utterance_id":"...","sample_rate":N,"channels":1}` | JSON | Utterance starting, audio format metadata |
| Binary | PCM16 LE bytes | Audio chunk (always aligned to 2-byte sample boundaries) |
| `{"type":"done","utterance_id":"..."}` | JSON | Utterance complete |
| `{"type":"cancelled","utterance_id":"..."}` | JSON | Utterance aborted by client |
| `{"type":"error","utterance_id":"...","message":"..."}` | JSON | Backend error (has utterance_id) |
| `{"type":"error","message":"..."}` | JSON | Client/protocol error (no utterance_id) |

## Configuration

All via environment variables:

| Variable | Default | Description |
|----------|---------|-------------|
| `BACKEND_URL` | `http://localhost:8000` | TTS backend base URL |
| `BACKEND_API_KEY` | — | Optional `Authorization: Bearer` token for backend |
| `TTS_DEFAULT_MODEL` | `kokoro` | Default model when client doesn't specify |
| `TTS_DEFAULT_VOICE` | `af_heart` | Default voice when client doesn't specify |
| `TTS_CHUNK_SIZE` | `4800` | Bytes per binary frame (4800 = 100ms at 24kHz mono). Must be even (PCM16 = 2 bytes/sample). |
| `MAX_BUFFER_SIZE` | `5242880` | Max buffered bytes per connection (5MB). Connection closed with error if exceeded. |
| `PORT` | `8000` | Server listen port |
| `LOG_LEVEL` | `info` | Log level (`debug`, `info`, `warn`/`warning`, `error`) |
| `LOG_FORMAT` | `json` | Log output format (`json` for structured logging, `plain` for human-readable). Default `json` is optimized for containerized deployments; use `plain` for local development. |

## Docker

```bash
# With a local Speaches instance
docker run -d \
  -e BACKEND_URL=http://speaches:8000 \
  -p 8080:8000 \
  ghcr.io/amenophis1er/tts-bridge

# With OpenAI
docker run -d \
  -e BACKEND_URL=https://api.openai.com \
  -e BACKEND_API_KEY=sk-... \
  -e TTS_DEFAULT_MODEL=tts-1 \
  -e TTS_DEFAULT_VOICE=alloy \
  -p 8080:8000 \
  ghcr.io/amenophis1er/tts-bridge
```

### Docker Compose (with Speaches + GPU)

```yaml
services:
  speaches:
    image: ghcr.io/speaches-ai/speaches:latest-cuda
    runtime: nvidia
    environment:
      - TTS__DEFAULT_MODEL=kokoro

  tts-bridge:
    image: ghcr.io/amenophis1er/tts-bridge
    environment:
      - BACKEND_URL=http://speaches:8000
    ports:
      - "8080:8000"
    depends_on:
      - speaches
```

## Health Check

`GET /health` returns:

```json
{ "status": "ok" }
```

Returns `503` if the backend is unreachable (proxy checks `GET {BACKEND_URL}/health` or `/v1/models`).

## Implementation

### Stack

- **Rust** (Edition 2024)
- **axum** — HTTP server + WebSocket upgrade
- **reqwest** — streaming HTTP client for backend requests
- **tokio** — async runtime
- **tracing** + **tracing-subscriber** — structured logging (JSON/plain)
- **~5MB static binary** (musl build), **~10MB Docker image** (distroless + CA certs)

### Why Rust

This proxy forwards bytes from an HTTP stream to a WebSocket — it's pure I/O. Rust with tokio gives:

- **High concurrency** — thousands of connections on a single core, zero-cost async
- **No GC pauses** — deterministic latency, no micro-stutters during audio streaming
- **Minimal memory** — small per-connection footprint
- **Tiny deployment** — static binary, no runtime dependencies
- **Zero-copy forwarding** — read from HTTP stream, write directly to WebSocket

For a real-time audio proxy where jitter matters, Rust removes the proxy itself as a bottleneck entirely.

### Files

```
src/
├── main.rs          # Entrypoint, config loading, tracing init, graceful shutdown
├── lib.rs           # Public API: AppState, build_router(), module re-exports
├── config.rs        # Environment variable parsing with defaults
├── proxy.rs         # WebSocket handler: text → stream audio → chunk into frames
└── health.rs        # GET /health with backend connectivity check
Cargo.toml
Dockerfile
```

### How it works

1. Client connects via WebSocket to `/v1/audio/stream`
2. Client sends a JSON text frame with `text` (+ optional voice/model/etc.)
3. Bridge generates an `utterance_id` (or uses client-provided one) and builds an OpenAI-compatible request body, forwarding all client-provided parameters:
   ```json
   {
     "model": "kokoro",
     "voice": "af_heart",
     "input": "Hello, how are you?",
     "response_format": "pcm",
     "speed": 1.0,
     "sample_rate": 24000,
     "language": "en"
   }
   ```
4. Bridge sends `POST {BACKEND_URL}/v1/audio/speech` — if the backend returns an error, an `error` frame is sent and no `start` frame is emitted
5. On a successful backend response, bridge sends a `start` frame with audio format metadata and reads the response as a `bytes_stream()`
6. As response chunks arrive, bridge fills a `TTS_CHUNK_SIZE` buffer and sends each full buffer as a binary WebSocket frame — all chunks are aligned to 2-byte boundaries (PCM16 sample alignment)
7. Concurrently, bridge listens for incoming `cancel` messages via a `tokio::select!` — if received, the reqwest response is dropped (aborting the HTTP stream) and a `cancelled` frame is sent
8. On stream completion, bridge flushes any remaining bytes (padded to even length if needed) and sends `done`
9. Bridge loops back, waiting for the next text message on the same WebSocket

### Backpressure

Each connection has a bounded internal buffer (`MAX_BUFFER_SIZE`, default 5MB). If audio bytes from the backend accumulate faster than they can be chunked and forwarded to the WebSocket, and the buffer exceeds `MAX_BUFFER_SIZE`, the connection is closed with an error frame.

In practice, `await`-ing on `ws_sender.send()` provides natural TCP backpressure — if the client can't consume fast enough, sends block, which pauses backend reads. The `MAX_BUFFER_SIZE` limit is a safety cap for pathological cases.

Incoming client WebSocket messages are limited to 1MB.

### Graceful Shutdown

On SIGTERM, the bridge:

1. Stops accepting new connections
2. Lets in-flight utterances finish (up to 10 seconds)
3. Force-closes remaining connections

This aligns with Docker's default SIGKILL delay (10s after SIGTERM) and ensures clean shutdowns in Docker/Kubernetes deployments.

### Docker Build

The image uses a **distroless** base (not scratch) to include CA certificates for HTTPS backends. Multi-stage build:

1. `rust:alpine` — build static musl binary
2. `gcr.io/distroless/static-debian12` — copy binary + CA certs

### CI/CD

GitHub Actions workflow (`.github/workflows/ci.yml`):

- **On every PR and push to `main`**: `cargo fmt --check`, `cargo clippy -- -D warnings`, `cargo test` (in parallel), then Docker build (verify only)
- **On `v*` tags**: build + push multi-arch Docker image (`linux/amd64` + `linux/arm64`) to GHCR

## Limitations

- **No audio codec conversion** — output is always PCM16 LE at the requested sample rate. Clients handle encoding (opus, mp3, etc.) if needed.
- **Sequential utterances** — one text→audio cycle at a time per WebSocket. Sending text during an active utterance returns an error. For concurrent streams, open multiple connections.
- **No inference** — this is purely a protocol adapter. All TTS work happens in the backend.
- **Single backend** — one proxy instance talks to one backend URL. Use a reverse proxy for multi-backend routing.
- **No authentication on the WebSocket** — handled upstream by your reverse proxy (Traefik, nginx, Cloudflare, etc.).

## Compatible Backends

Any server implementing `POST /v1/audio/speech` with streaming response support:

| Backend | Notes |
|---------|-------|
| [Speaches](https://github.com/speaches-ai/speaches) | Self-hosted, supports Kokoro/Piper/etc. |
| [OpenAI TTS](https://platform.openai.com/docs/api-reference/audio/createSpeech) | `tts-1`, `tts-1-hd` models |
| [Azure OpenAI](https://learn.microsoft.com/en-us/azure/ai-services/openai/) | Via OpenAI-compatible endpoint |
| [LocalAI](https://localai.io/) | Self-hosted, OpenAI-compatible |
| Any OpenAI-compatible server | Must support streaming response on `/v1/audio/speech` |
