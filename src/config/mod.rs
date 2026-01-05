//! Configuration management for ostt.
//!
//! This module handles loading and saving application configuration from TOML files,
//! as well as secure storage of API credentials. Configuration is stored in the
//! user's config directory, while credentials are stored with restricted permissions
//! in the user's local data directory.

pub mod file;
pub mod secrets;

pub use file::{AudioConfig, OsttConfig, TranscriptionOverrideConfig, VisualizationType};
pub use secrets::{
    clear_api_key, get_api_key, get_authorized_providers, get_selected_model, save_api_key,
    save_selected_model,
};

pub use file::save_config;

#[derive(Debug, Clone, Default)]
pub struct TranscriptionOverrides {
    pub provider: Option<String>,
    pub model: Option<String>,
    pub endpoint: Option<String>,
    pub api_key: Option<String>,
}

impl TranscriptionOverrides {
    pub fn is_configured(&self) -> bool {
        self.provider.is_some()
            || self.model.is_some()
            || self.endpoint.is_some()
            || self.api_key.is_some()
    }
}

pub fn load_transcription_overrides(config: &OsttConfig) -> TranscriptionOverrides {
    let provider = env_optional("OSTT_TRANSCRIPTION_PROVIDER")
        .or_else(|| config.transcription.provider.clone());
    let model = env_optional("OSTT_TRANSCRIPTION_MODEL")
        .or_else(|| config.transcription.model.clone());
    let endpoint = env_optional("OSTT_TRANSCRIPTION_ENDPOINT")
        .or_else(|| config.transcription.endpoint.clone());
    let api_key = env_optional("OSTT_TRANSCRIPTION_API_KEY")
        .or_else(|| config.transcription.api_key.clone());

    TranscriptionOverrides {
        provider,
        model,
        endpoint,
        api_key,
    }
}

fn env_optional(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .and_then(|value| {
            let trimmed = value.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_string())
            }
        })
}
