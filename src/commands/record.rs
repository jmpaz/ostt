//! Audio recording and transcription.
//!
//! Handles audio recording with real-time waveform visualization, optional transcription,
//! and history management. Supports external triggers via SIGUSR1 signal.

use crate::clipboard::copy_to_clipboard;
use crate::config;
use crate::history::HistoryManager;
use crate::recording::{AudioRecorder, OsttTui, RecordingCommand};
use crate::transcription;
use crate::transcription::TranscriptionAnimation;
use crate::ui::ErrorScreen;
use dirs;
use std::fs;

/// Handles audio recording and optional transcription.
///
/// Records audio with real-time waveform visualization, optionally transcribes the recording,
/// and saves to history. Supports external triggers via SIGUSR1 signal.
pub async fn handle_record() -> Result<(), anyhow::Error> {
    tracing::info!("=== ostt Audio Recorder Started ===");

    let config_data = match config::OsttConfig::load() {
        Ok(config) => config,
        Err(err) => {
            tracing::error!("Failed to load configuration: {}", err);
            let error_message = format!(
                "Configuration Error:\n\n{}\n\nPlease check your ~/.config/ostt/ostt.toml file and try again.",
                err
            );
            let mut error_screen = ErrorScreen::new()?;
            error_screen.show_error(&error_message)?;
            error_screen.cleanup()?;
            return Err(anyhow::anyhow!("Configuration error: {}", err));
        }
    };

    tracing::info!(
        "Configuration loaded: device={}, sample_rate={}Hz, peak_threshold={}%, reference_level={}dBFS",
        config_data.audio.device,
        config_data.audio.sample_rate,
        config_data.audio.peak_volume_threshold,
        config_data.audio.reference_level_db
    );

    let mut audio_recorder = AudioRecorder::new(config_data.audio.sample_rate, config_data.audio.device.clone());

    if let Err(e) = audio_recorder.start_recording() {
        tracing::error!("Failed to start recording: {}", e);
        let error_message = format!(
            "Recording Error:\n\n{}\n\nPlease check your audio configuration and try again.",
            e
        );
        let mut error_screen = ErrorScreen::new()?;
        error_screen.show_error(&error_message)?;
        error_screen.cleanup()?;
        return Err(e);
    }

    let actual_sample_rate = audio_recorder.get_sample_rate();
    let mut tui = OsttTui::new(
        actual_sample_rate,
        config_data.audio.peak_volume_threshold,
        config_data.audio.reference_level_db,
        config_data.audio.visualization,
    )
    .map_err(|e| anyhow::anyhow!("Failed to initialize UI: {e}"))?;

    let term = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let term_clone = term.clone();
    signal_hook::flag::register(signal_hook::consts::SIGUSR1, term_clone)
        .map_err(|e| anyhow::anyhow!("Failed to register signal handler: {e}"))?;

    tracing::debug!(
        "Entering recording loop. Press 'Enter' to transcribe or 'Escape'/'q' to cancel."
    );
    let mut frame_count = 0u64;
    let mut should_transcribe = false;

    loop {
        if term.load(std::sync::atomic::Ordering::Relaxed) {
            tracing::info!("Received SIGUSR1: transcribing via external trigger");
            should_transcribe = true;
            break;
        }

        match tui.handle_input() {
            Ok(RecordingCommand::Continue) => {
                frame_count += 1;
                if frame_count.is_multiple_of(60) {
                    let sample_count = audio_recorder.sample_count();
                    let duration_secs = sample_count as f32 / actual_sample_rate as f32;
                    tracing::debug!("Recording: {:.1}s recorded", duration_secs);
                }

                let samples = audio_recorder.get_samples();
                tui.render_waveform(&samples)
                    .map_err(|e| anyhow::anyhow!("Render failed: {e}"))?;
            }
            Ok(RecordingCommand::Transcribe) => {
                should_transcribe = true;
                break;
            }
            Ok(RecordingCommand::Cancel) => {
                break;
            }
            Ok(RecordingCommand::TogglePause) => {
                audio_recorder.toggle_pause();
                tui.is_paused = audio_recorder.is_paused();
                let samples = audio_recorder.get_samples();
                tui.render_waveform(&samples)
                    .map_err(|e| anyhow::anyhow!("Render failed: {e}"))?;
            }
            Err(e) => {
                tracing::error!("Input handling error: {}", e);
                return Err(anyhow::anyhow!("Input handling error: {e}"));
            }
        }
    }

    tracing::debug!("Stopping recording and saving audio...");
    let codec = config_data
        .audio
        .output_format
        .split_whitespace()
        .next()
        .unwrap_or("mp3");
    let extension = match codec {
        "libopus" => "ogg",
        "libvorbis" => "ogg",
        "flac" => "flac",
        "aac" => "m4a",
        "pcm_s16le" => "wav",
        _ => codec,
    };

    // Save to temp directory with ostt-recording prefix
    let temp_dir = std::env::temp_dir();
    let filename = format!("ostt-recording.{extension}");
    let filepath = temp_dir.join(&filename);

    audio_recorder
        .stop_recording(Some(filepath.clone()), &config_data.audio.output_format)
        .map_err(|e| {
            tracing::error!("Failed to save recording: {}", e);
            e
        })?;

    if should_transcribe {
        let filepath_str = filepath.to_string_lossy().to_string();
        match resolve_transcription_config(&config_data) {
            Ok(Some(transcription_config)) => {
                if let Err(e) = transcribe_recording_with_animation(
                    &mut tui,
                    transcription_config,
                    &filepath_str,
                )
                .await
                {
                    tracing::warn!("Transcription failed: {}", e);
                    eprintln!("Warning: Transcription failed: {e}");
                }
            }
            Ok(None) => {
                tracing::debug!("No transcription model configured");
                tui.cleanup().ok();
                let mut error_screen = ErrorScreen::new()?;
                error_screen.show_error(
                    "Error: No transcription model configured.\n\nSet OSTT_TRANSCRIPTION_MODEL (and OSTT_TRANSCRIPTION_API_KEY) or run 'ostt auth'.",
                )?;
                error_screen.cleanup()?;
            }
            Err(e) => {
                tracing::warn!("Failed to resolve transcription settings: {}", e);
                tui.cleanup().ok();
                let mut error_screen = ErrorScreen::new()?;
                error_screen.show_error(&format!(
                    "Error: Failed to configure transcription.\n\n{e}"
                ))?;
                error_screen.cleanup()?;
            }
        }
    }

    tui.cleanup()
        .map_err(|e| anyhow::anyhow!("Cleanup failed: {e}"))?;

    tracing::info!("=== ostt Audio Recorder Exited Successfully ===");
    Ok(())
}

fn resolve_transcription_config(
    config_data: &config::OsttConfig,
) -> anyhow::Result<Option<transcription::TranscriptionConfig>> {
    let overrides = config::load_transcription_overrides(config_data);
    let using_override = overrides.is_configured();
    let selected_model_id = config::get_selected_model().ok().flatten();

    let model_value = match overrides.model.clone().or(selected_model_id) {
        Some(value) => value,
        None => return Ok(None),
    };

    let provider_override = match overrides.provider.as_deref() {
        Some(raw) => Some(parse_provider_override(raw)?),
        None => None,
    };

    let (model, api_model_name_override, provider) =
        match transcription::TranscriptionModel::from_id(&model_value) {
            Some(model) => {
                if let Some(provider_override) = provider_override {
                    if provider_override != model.provider() {
                        return Err(anyhow::anyhow!(
                            "Override provider '{}' does not match model '{}'.",
                            provider_override.id(),
                            model_value
                        ));
                    }
                }
                let provider = model.provider();
                (model, None, provider)
            }
            None => {
                let provider = provider_override.unwrap_or_else(|| {
                    tracing::debug!(
                        "No provider override set for custom model '{}'; defaulting to OpenAI-compatible API.",
                        model_value
                    );
                    transcription::TranscriptionProvider::OpenAI
                });
                let model = transcription::TranscriptionModel::default_for_provider(&provider);
                (model, Some(model_value), provider)
            }
        };

    let api_key = if using_override {
        overrides.api_key.unwrap_or_default()
    } else {
        match config::get_api_key(provider.id())? {
            Some(key) => key,
            None => {
                return Err(anyhow::anyhow!(
                    "No API key for {}. Set OSTT_TRANSCRIPTION_API_KEY or run 'ostt auth'.",
                    provider.name()
                ));
            }
        }
    };

    let keywords = load_keywords()?;

    Ok(Some(transcription::TranscriptionConfig::new_with_overrides(
        model,
        api_key,
        keywords,
        config_data.providers.clone(),
        api_model_name_override,
        overrides.endpoint,
        using_override,
    )))
}

fn parse_provider_override(raw: &str) -> anyhow::Result<transcription::TranscriptionProvider> {
    let normalized = raw.trim().to_lowercase();
    transcription::TranscriptionProvider::from_id(&normalized).ok_or_else(|| {
        let providers = transcription::TranscriptionProvider::all()
            .iter()
            .map(|provider| provider.id())
            .collect::<Vec<_>>()
            .join(", ");
        anyhow::anyhow!(
            "Unknown transcription provider '{}'. Expected one of: {}.",
            raw,
            providers
        )
    })
}

fn load_keywords() -> anyhow::Result<Vec<String>> {
    let config_dir = dirs::home_dir()
        .ok_or_else(|| anyhow::anyhow!("Could not determine home directory"))?
        .join(".config")
        .join("ostt");
    let keywords_file = config_dir.join("keywords.txt");
    if keywords_file.exists() {
        let content = fs::read_to_string(&keywords_file)?;
        Ok(content
            .lines()
            .map(|line| line.trim().to_string())
            .filter(|line| !line.is_empty())
            .collect())
    } else {
        Ok(Vec::new())
    }
}

/// Transcribes an audio recording with animated progress indicator.
///
/// # Errors
/// - If transcription fails
async fn transcribe_recording_with_animation(
    tui: &mut OsttTui,
    transcription_config: transcription::TranscriptionConfig,
    audio_filename: &str,
) -> anyhow::Result<()> {
    tracing::debug!(
        "Starting transcription with model '{}' for file '{}'",
        transcription_config.model_label(),
        audio_filename
    );

    let mut animation = TranscriptionAnimation::new(80);

    let filename = audio_filename.to_string();
    let transcription_handle = tokio::spawn(async move {
        transcription::transcribe(&transcription_config, filename.as_ref()).await
    });

    loop {
        if let Err(e) = tui.render_transcription_animation(&mut animation) {
            tracing::warn!("Failed to render animation: {}", e);
        }

        if transcription_handle.is_finished() {
            break;
        }

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }

    match transcription_handle.await {
        Ok(Ok(text)) => {
            tracing::info!("Transcription completed: {}", text);

            let data_dir = dirs::home_dir()
                .ok_or_else(|| anyhow::anyhow!("Could not determine home directory"))?
                .join(".local")
                .join("share")
                .join("ostt");

            let mut history_manager = HistoryManager::new(&data_dir)?;
            if let Err(e) = history_manager.save_transcription(&text) {
                tracing::warn!("Failed to save transcription to history: {}", e);
            }

            match copy_to_clipboard(&text) {
                Ok(_) => {
                    tracing::debug!("Transcribed text copied to clipboard");
                }
                Err(e) => {
                    tracing::warn!("Failed to copy to clipboard: {}", e);
                }
            }

            Ok(())
        }
        Ok(Err(e)) => {
            tracing::error!("Transcription failed: {}", e);
            tui.cleanup().ok();
            let mut error_screen = ErrorScreen::new()?;
            error_screen.show_error(&format!("Error: Transcription failed - {e}"))?;
            error_screen.cleanup()?;
            Err(e)
        }
        Err(e) => {
            tracing::error!("Transcription task failed: {}", e);
            tui.cleanup().ok();
            let mut error_screen = ErrorScreen::new()?;
            error_screen.show_error(&format!("Error: Transcription task failed - {e}"))?;
            error_screen.cleanup()?;
            Err(anyhow::anyhow!("Transcription task failed: {e}"))
        }
    }
}
