//! Audio recording and transcription.
//!
//! Handles audio recording with real-time waveform visualization, optional transcription,
//! and history management. Supports external triggers via SIGUSR1 signal.

use crate::clipboard::copy_to_clipboard;
use crate::config;
use crate::history::HistoryManager;
use crate::remote;
use crate::recording::{AudioRecorder, OsttTui, RecordingCommand};
use crate::transcription;
use crate::transcription::TranscriptionAnimation;
use crate::ui::ErrorScreen;
use dirs;
use std::fs;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::AsyncWriteExt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordingMode {
    Interactive,
    Remote,
}

struct RemoteSocketGuard {
    path: Option<PathBuf>,
}

impl RemoteSocketGuard {
    fn new() -> Self {
        Self { path: None }
    }

    fn set(&mut self, path: PathBuf) {
        self.path = Some(path);
    }
}

impl Drop for RemoteSocketGuard {
    fn drop(&mut self) {
        if let Some(path) = &self.path {
            remote::cleanup_socket(path);
        }
    }
}

/// Handles audio recording and optional transcription.
///
/// Records audio with real-time waveform visualization, optionally transcribes the recording,
/// and saves to history. Supports external triggers via SIGUSR1 signal and remote IPC.
pub async fn handle_record(mode: RecordingMode) -> Result<(), anyhow::Error> {
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

    let mut remote_rx = None;
    let mut remote_socket_guard = RemoteSocketGuard::new();
    if mode == RecordingMode::Remote {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        match remote::start_listener(tx).await {
            Ok(path) => {
                tracing::info!("Remote control listening on {}", path.display());
                remote_rx = Some(rx);
                remote_socket_guard.set(path);
            }
            Err(err) => {
                tracing::error!("Failed to start remote listener: {err}");
                let mut error_screen = ErrorScreen::new()?;
                error_screen.show_error(&format!(
                    "Remote Control Error:\n\n{err}\n\nClose any running ostt remote instance and try again."
                ))?;
                error_screen.cleanup()?;
                return Err(anyhow::anyhow!("Remote listener error: {err}"));
            }
        }
    }

    let mut audio_recorder =
        AudioRecorder::new(config_data.audio.sample_rate, config_data.audio.device.clone());

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
    let mut typing_error = None;

    'recording: loop {
        if let Some(rx) = remote_rx.as_mut() {
            if let Ok(signal) = rx.try_recv() {
                match signal {
                    remote::RemoteSignal::Complete => {
                        tracing::info!("Received remote completion command");
                        should_transcribe = true;
                        break 'recording;
                    }
                    remote::RemoteSignal::Cancel => {
                        tracing::info!("Received remote cancel command");
                        break 'recording;
                    }
                }
            }
        }

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
                match transcribe_recording_with_animation(
                    &mut tui,
                    transcription_config,
                    &filepath_str,
                )
                .await
                {
                    Ok(text) => {
                        if mode == RecordingMode::Remote {
                            if let Err(err) =
                                type_transcription_with_feedback(&mut tui, &text).await
                            {
                                tracing::warn!("Typing failed: {}", err);
                                typing_error = Some(err);
                            }
                        }
                    }
                    Err(e) => {
                        tracing::warn!("Transcription failed: {}", e);
                        eprintln!("Warning: Transcription failed: {e}");
                    }
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

    if let Some(err) = typing_error {
        tui.cleanup().ok();
        let mut error_screen = ErrorScreen::new()?;
        error_screen.show_error(&format!(
            "Error: Failed to type transcription.\n\n{err}"
        ))?;
        error_screen.cleanup()?;
        return Err(err);
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
) -> anyhow::Result<String> {
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

            Ok(text)
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

async fn type_transcription_with_feedback(
    tui: &mut OsttTui,
    text: &str,
) -> anyhow::Result<()> {
    let chars: Vec<char> = text.chars().collect();
    let total = chars.len();
    let delay_ms = read_type_delay_ms();
    let start_delay_ms = read_type_start_delay_ms();

    tui.render_typing_progress(text, 0)
        .map_err(|e| anyhow::anyhow!("Typing UI error: {e}"))?;

    if total == 0 {
        return Ok(());
    }

    let ydotool_bin = ydotool_bin();
    let mut child = tokio::process::Command::new(ydotool_bin)
        .args(["type", "--file", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| anyhow::anyhow!("Failed to start ydotool: {e}"))?;

    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| anyhow::anyhow!("Failed to open ydotool stdin"))?;

    if start_delay_ms > 0 {
        tokio::time::sleep(Duration::from_millis(start_delay_ms)).await;
    }

    for (idx, ch) in chars.iter().enumerate() {
        let mut buf = [0u8; 4];
        let slice = ch.encode_utf8(&mut buf);
        stdin.write_all(slice.as_bytes()).await?;
        stdin.flush().await?;
        tui.render_typing_progress(text, idx + 1)
            .map_err(|e| anyhow::anyhow!("Typing UI error: {e}"))?;

        if delay_ms > 0 {
            tokio::time::sleep(Duration::from_millis(delay_ms)).await;
        }
    }

    drop(stdin);
    let status = child.wait().await?;
    if !status.success() {
        return Err(anyhow::anyhow!("ydotool exited with status {status}"));
    }

    Ok(())
}

fn read_type_delay_ms() -> u64 {
    std::env::var("OSTT_REMOTE_TYPE_DELAY_MS")
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .unwrap_or(8)
}

fn read_type_start_delay_ms() -> u64 {
    std::env::var("OSTT_REMOTE_TYPE_START_DELAY_MS")
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .unwrap_or(120)
}

fn ydotool_bin() -> String {
    std::env::var("OSTT_YDOTOOL_BIN")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "ydotool".to_string())
}
