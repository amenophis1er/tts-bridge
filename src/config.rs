use std::env;

#[derive(Debug, Clone)]
pub enum LogFormat {
    Json,
    Plain,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub backend_url: String,
    pub backend_api_key: Option<String>,
    pub default_model: String,
    pub default_voice: String,
    pub chunk_size: usize,
    pub max_buffer_size: usize,
    pub port: u16,
    pub log_level: String,
    pub log_format: LogFormat,
}

impl Config {
    pub fn from_env() -> Self {
        let chunk_size: usize = env::var("TTS_CHUNK_SIZE")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(4800);

        if chunk_size == 0 || !chunk_size.is_multiple_of(2) {
            eprintln!(
                "FATAL: TTS_CHUNK_SIZE must be a positive even number (PCM16 = 2 bytes/sample), got {chunk_size}"
            );
            std::process::exit(1);
        }

        let log_format = match env::var("LOG_FORMAT")
            .unwrap_or_else(|_| "json".into())
            .to_lowercase()
            .as_str()
        {
            "plain" => LogFormat::Plain,
            _ => LogFormat::Json,
        };

        Config {
            backend_url: env::var("BACKEND_URL").unwrap_or_else(|_| "http://localhost:8000".into()),
            backend_api_key: env::var("BACKEND_API_KEY").ok(),
            default_model: env::var("TTS_DEFAULT_MODEL").unwrap_or_else(|_| "kokoro".into()),
            default_voice: env::var("TTS_DEFAULT_VOICE").unwrap_or_else(|_| "af_heart".into()),
            chunk_size,
            max_buffer_size: env::var("MAX_BUFFER_SIZE")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(5_242_880),
            port: env::var("PORT")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(8000),
            log_level: normalize_log_level(
                &env::var("LOG_LEVEL").unwrap_or_else(|_| "info".into()),
            ),
            log_format,
        }
    }
}

/// Normalize log level aliases: the spec documents "warning" but tracing uses "warn".
fn normalize_log_level(level: &str) -> String {
    match level.to_lowercase().as_str() {
        "warning" => "warn".into(),
        other => other.into(),
    }
}
