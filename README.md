# tts-bridge

WebSocket streaming proxy for any OpenAI-compatible TTS API.

```
Voice App ──WS──► tts-bridge ──HTTP (streaming)──► TTS Backend
                  (this project)                    (Speaches, OpenAI, etc.)
```

Send text over WebSocket, receive PCM16 audio chunks in real-time. Persistent connection with multiple utterances, mid-stream cancellation, and sticky session parameters. Zero inference, zero GPU — just protocol translation.

## Quick Start

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

## How It Works

1. Client connects via WebSocket to `/v1/audio/stream`
2. Client sends a JSON text frame with `text` (+ optional voice/model/etc.)
3. Bridge sends `POST /v1/audio/speech` to the backend with streaming response
4. Audio bytes are chunked into fixed-size PCM16 frames and forwarded as binary WebSocket frames
5. Client receives `start` → binary audio chunks → `done` for each utterance
6. Connection stays open for the next utterance

### Cancellation (Barge-in)

Send `{"type":"cancel"}` during streaming to immediately abort the backend request. The bridge sends a `cancelled` frame and is ready for the next utterance.

### Sticky Parameters

Voice, model, sample rate, language, and speed carry over between utterances on the same connection. Override any of them per-message, or send `{"type":"reset"}` to return to defaults.

## Configuration

All via environment variables:

| Variable | Default | Description |
|----------|---------|-------------|
| `BACKEND_URL` | `http://localhost:8000` | TTS backend base URL |
| `BACKEND_API_KEY` | — | Optional `Authorization: Bearer` token for backend |
| `TTS_DEFAULT_MODEL` | `kokoro` | Default model when client doesn't specify |
| `TTS_DEFAULT_VOICE` | `af_heart` | Default voice when client doesn't specify |
| `TTS_CHUNK_SIZE` | `4800` | Bytes per binary frame (4800 = 100ms at 24kHz mono). Must be even. |
| `MAX_BUFFER_SIZE` | `5242880` | Max buffered bytes per connection (5MB) |
| `PORT` | `8000` | Server listen port |
| `LOG_LEVEL` | `info` | Log level (`debug`, `info`, `warn`/`warning`, `error`) |
| `LOG_FORMAT` | `json` | Log output format (`json` or `plain`) |

Incoming client WebSocket messages are limited to 1MB. Connections exceeding `MAX_BUFFER_SIZE` of unsent audio data are closed with an error frame.

## Protocol

### Client → Bridge (JSON text frames)

**Synthesize:**
```json
{
  "text": "Hello, how are you?",
  "voice": "af_heart",
  "model": "kokoro",
  "sample_rate": 24000,
  "language": "en",
  "speed": 1.0,
  "utterance_id": "my-id-123"
}
```
Only `text` is required. All other fields are optional with sticky defaults.

**Cancel:** `{"type":"cancel"}` — abort current utterance

**Reset:** `{"type":"reset"}` — clear sticky parameters to server defaults

### Bridge → Client

| Frame | Type | Description |
|-------|------|-------------|
| `{"type":"start","utterance_id":"...","sample_rate":N,"channels":1}` | JSON | Utterance starting |
| Binary | PCM16 LE bytes | Audio chunk |
| `{"type":"done","utterance_id":"..."}` | JSON | Utterance complete |
| `{"type":"cancelled","utterance_id":"..."}` | JSON | Utterance aborted |
| `{"type":"error","message":"..."}` | JSON | Error (may include `utterance_id`) |

See [PROTOCOL.md](PROTOCOL.md) for the full specification.

## Try It

A self-contained browser demo is included — no build step, just open the file:

1. Start the stack: `docker compose up`
2. Open [`examples/web-client/index.html`](examples/web-client/index.html) in your browser
3. Type text, click **Speak**, hear audio. Click **Cancel** for barge-in.

## Docker Compose

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

`GET /health` returns `200 {"status":"ok"}` when the backend is reachable, or `503 {"status":"error","message":"..."}` otherwise.

## Compatible Backends

Any server implementing `POST /v1/audio/speech` with streaming response support:

| Backend | Notes |
|---------|-------|
| [Speaches](https://github.com/speaches-ai/speaches) | Self-hosted, supports Kokoro/Piper/etc. |
| [OpenAI TTS](https://platform.openai.com/docs/api-reference/audio/createSpeech) | `tts-1`, `tts-1-hd` models |
| [Azure OpenAI](https://learn.microsoft.com/en-us/azure/ai-services/openai/) | Via OpenAI-compatible endpoint |
| [LocalAI](https://localai.io/) | Self-hosted, OpenAI-compatible |

## Building from Source

```bash
cargo build --release
```

## Contributing

Contributions are welcome! Please open an issue or submit a pull request.

## License

[MIT](LICENSE)
