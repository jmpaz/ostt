use crate::clipboard::copy_to_clipboard;
use crate::config;
use crate::history::HistoryManager;
use crate::recording::{AudioRecorder, OsttTui, RecordingCommand, TranscriptionAnimation};
use crate::remote;
use crate::ui::ErrorScreen;
use chrono::Utc;
use std::ffi::OsString;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::time::{Duration, SystemTime};
use tokio::io::AsyncWriteExt;

const CONTEXTUALIZE_BIN: &str = "contextualize";
const CONTEXTUALIZE_CACHE_TTL: &str = "7d";
const RECORDING_CACHE_RETENTION: Duration = Duration::from_secs(7 * 24 * 60 * 60);
const DEFAULT_TRANSCRIPTION_TIMEOUT: Duration = Duration::from_secs(120);

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

    let mut audio_recorder = AudioRecorder::new(
        config_data.audio.sample_rate,
        config_data.audio.device.clone(),
    );

    if let Err(err) = audio_recorder.start_recording() {
        tracing::error!("Failed to start recording: {}", err);
        let error_message = format!(
            "Recording Error:\n\n{}\n\nPlease check your audio configuration and try again.",
            err
        );
        let mut error_screen = ErrorScreen::new()?;
        error_screen.show_error(&error_message)?;
        error_screen.cleanup()?;
        return Err(err);
    }

    let actual_sample_rate = audio_recorder.get_sample_rate();
    let mut tui = OsttTui::new(
        actual_sample_rate,
        config_data.audio.peak_volume_threshold,
        config_data.audio.reference_level_db,
        config_data.audio.visualization,
    )
    .map_err(|err| anyhow::anyhow!("Failed to initialize UI: {err}"))?;

    let term = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let term_clone = term.clone();
    signal_hook::flag::register(signal_hook::consts::SIGUSR1, term_clone)
        .map_err(|err| anyhow::anyhow!("Failed to register signal handler: {err}"))?;

    tracing::debug!(
        "Entering recording loop. Press 'Enter' to transcribe or 'Escape'/'q' to cancel."
    );
    let mut frame_count = 0u64;
    let mut should_transcribe = false;
    let mut typing_error = None;
    let mut remote_output_override = None;
    let mut cancel_pending = false;

    'recording: loop {
        if cancel_pending {
            if let Ok(RecordingCommand::Cancel) = tui.handle_input() {
                break 'recording;
            }

            let samples = audio_recorder.get_samples();
            tui.render_waveform(&samples)
                .map_err(|err| anyhow::anyhow!("Render failed: {err}"))?;
            if tui.cancel_animation_done() {
                break 'recording;
            }
            continue;
        }

        if let Some(rx) = remote_rx.as_mut() {
            if let Ok(signal) = rx.try_recv() {
                match signal {
                    remote::RemoteSignal::Complete(mode) => {
                        tracing::info!("Received remote completion command");
                        remote_output_override = mode;
                        should_transcribe = true;
                        break 'recording;
                    }
                    remote::RemoteSignal::Cancel => {
                        tracing::info!("Received remote cancel command");
                        tui.start_cancel_animation();
                        cancel_pending = true;
                        continue;
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
                    .map_err(|err| anyhow::anyhow!("Render failed: {err}"))?;
            }
            Ok(RecordingCommand::Transcribe) => {
                should_transcribe = true;
                break;
            }
            Ok(RecordingCommand::Cancel) => {
                tui.start_cancel_animation();
                cancel_pending = true;
                continue;
            }
            Ok(RecordingCommand::TogglePause) => {
                audio_recorder.toggle_pause();
                tui.is_paused = audio_recorder.is_paused();
                let samples = audio_recorder.get_samples();
                tui.render_waveform(&samples)
                    .map_err(|err| anyhow::anyhow!("Render failed: {err}"))?;
            }
            Err(err) => {
                tracing::error!("Input handling error: {}", err);
                return Err(anyhow::anyhow!("Input handling error: {err}"));
            }
        }
    }

    tracing::debug!("Stopping recording and saving audio...");
    let extension = recording_extension(&config_data.audio.output_format);
    let recordings_dir = recordings_cache_dir()?;
    prune_recording_cache(
        &recordings_dir,
        SystemTime::now(),
        RECORDING_CACHE_RETENTION,
    );
    let filepath = recordings_dir.join(recording_cache_filename(extension));

    if should_transcribe {
        match finalize_and_transcribe_with_animation(
            &mut tui,
            audio_recorder,
            filepath.clone(),
            config_data.audio.output_format.clone(),
        )
        .await
        {
            Ok(text) => {
                if mode == RecordingMode::Remote {
                    if let Err(err) =
                        output_transcription_with_feedback(&mut tui, &text, remote_output_override)
                            .await
                    {
                        tracing::warn!("Remote output failed: {}", err);
                        typing_error = Some(err);
                    }
                } else if let Err(err) = copy_to_clipboard(&text) {
                    tracing::warn!("Failed to copy to clipboard: {}", err);
                } else {
                    tracing::debug!("Transcribed text copied to clipboard");
                }
            }
            Err(err) => {
                tracing::warn!("Transcription failed: {}", err);
                eprintln!("Warning: Transcription failed: {err}");
            }
        }
    } else {
        audio_recorder
            .stop_recording(Some(filepath.clone()), &config_data.audio.output_format)
            .map_err(|err| {
                tracing::error!("Failed to save recording: {}", err);
                err
            })?;
    }

    if let Some(err) = typing_error {
        tui.cleanup().ok();
        let mut error_screen = ErrorScreen::new()?;
        error_screen.show_error(&format!("Error: Failed to type transcription.\n\n{err}"))?;
        error_screen.cleanup()?;
        return Err(err);
    }

    tui.cleanup()
        .map_err(|err| anyhow::anyhow!("Cleanup failed: {err}"))?;

    tracing::info!("=== ostt Audio Recorder Exited Successfully ===");
    Ok(())
}

fn ostt_data_dir() -> anyhow::Result<PathBuf> {
    let data_dir = dirs::home_dir()
        .ok_or_else(|| anyhow::anyhow!("Could not determine home directory"))?
        .join(".local")
        .join("share")
        .join("ostt");
    fs::create_dir_all(&data_dir)?;
    Ok(data_dir)
}

fn ostt_state_dir() -> anyhow::Result<PathBuf> {
    let state_dir = if let Ok(xdg_state) = std::env::var("XDG_STATE_HOME") {
        PathBuf::from(xdg_state).join("ostt")
    } else {
        dirs::home_dir()
            .ok_or_else(|| anyhow::anyhow!("Could not determine home directory"))?
            .join(".local")
            .join("state")
            .join("ostt")
    };
    fs::create_dir_all(&state_dir)?;
    Ok(state_dir)
}

fn transcription_debug_log_path() -> anyhow::Result<PathBuf> {
    Ok(ostt_state_dir()?.join("transcription-latest.log"))
}

fn reset_transcription_debug_log(audio_path: &Path) {
    let path = match transcription_debug_log_path() {
        Ok(path) => path,
        Err(err) => {
            tracing::debug!("Failed to resolve transcription debug log path: {}", err);
            return;
        }
    };

    let mut file = match fs::File::create(&path) {
        Ok(file) => file,
        Err(err) => {
            tracing::debug!("Failed to create transcription debug log: {}", err);
            return;
        }
    };

    let _ = writeln!(file, "timestamp={}", Utc::now().to_rfc3339());
    let _ = writeln!(file, "audio_path={}", audio_path.display());
    let _ = writeln!(file, "contextualize_bin={}", contextualize_bin());
    let _ = writeln!(
        file,
        "contextualize_timeout_seconds={}",
        read_transcription_timeout().as_secs()
    );
    let _ = writeln!(
        file,
        "whisper_url={}",
        std::env::var("WHISPER_URL").unwrap_or_default()
    );
    let _ = writeln!(
        file,
        "whisper_api_base={}",
        std::env::var("WHISPER_API_BASE").unwrap_or_default()
    );
    let _ = writeln!(
        file,
        "whisper_model={}",
        std::env::var("WHISPER_MODEL").unwrap_or_default()
    );
}

fn append_transcription_debug_log(message: impl AsRef<str>) {
    let path = match transcription_debug_log_path() {
        Ok(path) => path,
        Err(err) => {
            tracing::debug!("Failed to resolve transcription debug log path: {}", err);
            return;
        }
    };

    let mut file = match fs::OpenOptions::new().create(true).append(true).open(&path) {
        Ok(file) => file,
        Err(err) => {
            tracing::debug!("Failed to append transcription debug log: {}", err);
            return;
        }
    };

    let _ = writeln!(file, "{} {}", Utc::now().to_rfc3339(), message.as_ref());
}

fn keywords_file() -> anyhow::Result<PathBuf> {
    Ok(dirs::home_dir()
        .ok_or_else(|| anyhow::anyhow!("Could not determine home directory"))?
        .join(".config")
        .join("ostt")
        .join("keywords.txt"))
}

fn recordings_cache_dir() -> anyhow::Result<PathBuf> {
    let cache_dir = ostt_data_dir()?.join("cache").join("recordings");
    fs::create_dir_all(&cache_dir)?;
    Ok(cache_dir)
}

fn recording_extension(output_format: &str) -> &str {
    match output_format.split_whitespace().next().unwrap_or("mp3") {
        "libopus" | "libvorbis" => "ogg",
        "flac" => "flac",
        "aac" => "m4a",
        "pcm_s16le" => "wav",
        codec => codec,
    }
}

fn recording_cache_filename(extension: &str) -> String {
    let now = Utc::now();
    format!(
        "{}-{}-{:03}.{}",
        now.format("%Y%m%dT%H%M%SZ"),
        std::process::id(),
        now.timestamp_subsec_millis(),
        extension
    )
}

fn prune_recording_cache(dir: &Path, now: SystemTime, retention: Duration) {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(err) => {
            tracing::debug!("Skipping recording cache prune: {}", err);
            return;
        }
    };

    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if !file_type.is_file() {
            continue;
        }

        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        let Ok(modified) = metadata.modified() else {
            continue;
        };
        let Ok(age) = now.duration_since(modified) else {
            continue;
        };
        if age <= retention {
            continue;
        }

        if let Err(err) = fs::remove_file(&path) {
            tracing::debug!(
                "Failed to remove expired recording cache entry {}: {}",
                path.display(),
                err
            );
        }
    }
}

fn build_contextualize_args(audio_path: &Path, keywords_path: Option<&Path>) -> Vec<OsString> {
    let mut args = vec![
        OsString::from("--verbose"),
        OsString::from("cat"),
        OsString::from("-f"),
        OsString::from("raw"),
        OsString::from("--cache-ttl"),
        OsString::from(CONTEXTUALIZE_CACHE_TTL),
    ];

    if let Some(path) = keywords_path {
        args.push(OsString::from("--transcribe-prompt-file"));
        args.push(path.as_os_str().to_os_string());
    }

    args.push(audio_path.as_os_str().to_os_string());
    args
}

async fn transcribe_recording_with_animation(
    tui: &mut OsttTui,
    audio_path: &Path,
) -> anyhow::Result<String> {
    if !audio_path.exists() {
        return Err(anyhow::anyhow!("No audio was captured."));
    }

    let contextualize = contextualize_bin();
    let timeout = read_transcription_timeout();
    reset_transcription_debug_log(audio_path);
    tracing::debug!(
        "Starting contextualize transcription for file '{}' with timeout {:?}",
        audio_path.display(),
        timeout
    );
    append_transcription_debug_log(format!(
        "phase=transcribe-start audio_path={} timeout_seconds={}",
        audio_path.display(),
        timeout.as_secs()
    ));

    let keywords_path = keywords_file()?;
    let args = build_contextualize_args(
        audio_path,
        keywords_path.exists().then_some(keywords_path.as_path()),
    );
    let mut animation = TranscriptionAnimation::new();

    let transcription_handle =
        tokio::spawn(
            async move { run_transcription_command(&contextualize, &args, timeout).await },
        );

    loop {
        if let Err(err) = tui.render_transcription_animation(&mut animation) {
            tracing::warn!("Failed to render animation: {}", err);
        }

        if transcription_handle.is_finished() {
            break;
        }

        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    match transcription_handle.await {
        Ok(Ok(text)) => {
            tracing::info!("Transcription completed: {}", text);
            append_transcription_debug_log(format!(
                "phase=transcribe-finished transcript_chars={}",
                text.chars().count()
            ));

            let mut history_manager = HistoryManager::new(&ostt_data_dir()?)?;
            if let Err(err) = history_manager.save_transcription(&text) {
                tracing::warn!("Failed to save transcription to history: {}", err);
            }

            Ok(text)
        }
        Ok(Err(err)) => {
            tracing::error!("Transcription failed: {}", err);
            append_transcription_debug_log(format!("phase=transcribe-error error={}", err));
            tui.cleanup().ok();
            let mut error_screen = ErrorScreen::new()?;
            error_screen.show_error(&format!("Error: Transcription failed - {err}"))?;
            error_screen.cleanup()?;
            Err(err)
        }
        Err(err) => {
            tracing::error!("Transcription task failed: {}", err);
            append_transcription_debug_log(format!("phase=transcribe-task-error error={}", err));
            tui.cleanup().ok();
            let mut error_screen = ErrorScreen::new()?;
            error_screen.show_error(&format!("Error: Transcription task failed - {err}"))?;
            error_screen.cleanup()?;
            Err(anyhow::anyhow!("Transcription task failed: {err}"))
        }
    }
}

async fn finalize_and_transcribe_with_animation(
    tui: &mut OsttTui,
    mut audio_recorder: AudioRecorder,
    audio_path: PathBuf,
    output_format: String,
) -> anyhow::Result<String> {
    append_transcription_debug_log("phase=finalize-start");
    let save_handle = tokio::task::spawn_blocking(move || {
        let recording = audio_recorder.finish_recording()?;
        recording.save(Some(audio_path.clone()), &output_format)?;
        Ok::<PathBuf, anyhow::Error>(audio_path)
    });

    let mut animation = TranscriptionAnimation::new();

    loop {
        if let Err(err) = tui.render_transcription_animation(&mut animation) {
            tracing::warn!("Failed to render animation: {}", err);
        }

        if save_handle.is_finished() {
            break;
        }

        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    let audio_path = match save_handle.await {
        Ok(Ok(path)) => {
            append_transcription_debug_log(format!(
                "phase=finalize-finished audio_path={}",
                path.display()
            ));
            path
        }
        Ok(Err(err)) => {
            tracing::error!("Failed to save recording: {}", err);
            append_transcription_debug_log(format!("phase=finalize-error error={}", err));
            tui.cleanup().ok();
            let mut error_screen = ErrorScreen::new()?;
            error_screen.show_error(&format!("Error: Failed to save recording - {err}"))?;
            error_screen.cleanup()?;
            return Err(err);
        }
        Err(err) => {
            tracing::error!("Recording save task failed: {}", err);
            append_transcription_debug_log(format!("phase=finalize-task-error error={}", err));
            tui.cleanup().ok();
            let mut error_screen = ErrorScreen::new()?;
            error_screen.show_error(&format!("Error: Recording save task failed - {err}"))?;
            error_screen.cleanup()?;
            return Err(anyhow::anyhow!("Recording save task failed: {err}"));
        }
    };

    transcribe_recording_with_animation(tui, &audio_path).await
}

async fn run_transcription_command(
    program: &str,
    args: &[OsString],
    timeout_duration: Duration,
) -> anyhow::Result<String> {
    tracing::debug!("Running transcription command: {} {:?}", program, args);
    append_transcription_debug_log(format!(
        "phase=subprocess-spawn program={} args={:?}",
        program, args
    ));

    let output = tokio::time::timeout(
        timeout_duration,
        tokio::process::Command::new(program)
            .kill_on_drop(true)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output(),
    )
    .await
    .map_err(|_| {
        append_transcription_debug_log(format!(
            "phase=subprocess-timeout program={} timeout_seconds={}",
            program,
            timeout_duration.as_secs().max(1)
        ));
        anyhow::anyhow!(
            "{} timed out after {}s. Set OSTT_CONTEXTUALIZE_TIMEOUT_SECONDS to allow longer transcriptions.",
            program,
            timeout_duration.as_secs().max(1)
        )
    })?
    .map_err(|err| {
        append_transcription_debug_log(format!(
            "phase=subprocess-spawn-error program={} error={}",
            program, err
        ));
        if err.kind() == std::io::ErrorKind::NotFound {
            anyhow::anyhow!(
                "Could not find '{}' on PATH. Install contextualize before recording.",
                program
            )
        } else {
            anyhow::anyhow!("Failed to run '{}': {}", program, err)
        }
    })?;

    parse_transcription_output(program, &output.stdout, &output.stderr, output.status)
}

fn parse_transcription_output(
    program: &str,
    stdout: &[u8],
    stderr: &[u8],
    status: ExitStatus,
) -> anyhow::Result<String> {
    let stderr_text = String::from_utf8_lossy(stderr).trim().to_string();
    append_transcription_debug_log(format!(
        "phase=subprocess-exit success={} status={} stdout_bytes={} stderr_bytes={}",
        status.success(),
        status,
        stdout.len(),
        stderr.len()
    ));
    if !stderr_text.is_empty() {
        append_transcription_debug_log(format!("stderr={}", stderr_text));
    }
    if !status.success() {
        if stderr_text.is_empty() {
            return Err(anyhow::anyhow!("{} exited with status {}", program, status));
        }
        return Err(anyhow::anyhow!("{} failed: {}", program, stderr_text));
    }

    let stdout_text = String::from_utf8(stdout.to_vec())
        .map_err(|err| anyhow::anyhow!("{} returned invalid UTF-8 on stdout: {}", program, err))?;
    let transcript = stdout_text.trim().to_string();
    if transcript.is_empty() {
        if stderr_text.is_empty() {
            return Err(anyhow::anyhow!("{} returned no transcript.", program));
        }
        return Err(anyhow::anyhow!(
            "{} returned no transcript. {}",
            program,
            stderr_text
        ));
    }

    Ok(transcript)
}

async fn output_transcription_with_feedback(
    tui: &mut OsttTui,
    text: &str,
    output_override: Option<remote::RemoteOutputMode>,
) -> anyhow::Result<()> {
    match resolve_output_mode(output_override) {
        remote::RemoteOutputMode::Paste => {
            paste_transcription(text).await?;
            let header = paste_header_label();
            stream_transcription_preview(tui, text, &header).await?;
        }
        remote::RemoteOutputMode::Type => {
            if let Err(err) = copy_to_clipboard(text) {
                tracing::warn!("Failed to copy to clipboard before typing: {}", err);
            }
            let header = type_header_label();
            type_transcription_with_feedback(tui, text, &header).await?;
        }
    }

    Ok(())
}

async fn type_transcription_with_feedback(
    tui: &mut OsttTui,
    text: &str,
    header_label: &str,
) -> anyhow::Result<()> {
    let chars: Vec<char> = text.chars().collect();
    let total = chars.len();
    let delay_ms = read_type_delay_ms();
    let start_delay_ms = read_type_start_delay_ms();

    tui.render_typing_progress(text, 0, header_label)
        .map_err(|err| anyhow::anyhow!("Typing UI error: {err}"))?;

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
        .map_err(|err| anyhow::anyhow!("Failed to start ydotool: {err}"))?;

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
        tui.render_typing_progress(text, idx + 1, header_label)
            .map_err(|err| anyhow::anyhow!("Typing UI error: {err}"))?;

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

async fn stream_transcription_preview(
    tui: &mut OsttTui,
    text: &str,
    header_label: &str,
) -> anyhow::Result<()> {
    let chars: Vec<char> = text.chars().collect();
    let total = chars.len();
    tui.render_typing_progress(text, 0, header_label)
        .map_err(|err| anyhow::anyhow!("Typing UI error: {err}"))?;

    if total == 0 {
        return Ok(());
    }

    let duration_ms = read_stream_duration_ms();
    let frame_ms = 16u64;
    let frames = ((duration_ms / frame_ms).max(1)) as usize;
    let step = total.div_ceil(frames);

    let mut typed = 0usize;
    for _ in 0..frames {
        typed = (typed + step).min(total);
        tui.render_typing_progress(text, typed, header_label)
            .map_err(|err| anyhow::anyhow!("Typing UI error: {err}"))?;
        tokio::time::sleep(Duration::from_millis(frame_ms)).await;
    }

    if typed < total {
        tui.render_typing_progress(text, total, header_label)
            .map_err(|err| anyhow::anyhow!("Typing UI error: {err}"))?;
    }

    Ok(())
}

async fn paste_transcription(text: &str) -> anyhow::Result<()> {
    #[cfg(target_os = "linux")]
    {
        match paste_transcription_with_wrapctl(text).await {
            Ok(()) => return Ok(()),
            Err(err) if allow_ydotool_fallback() => {
                tracing::warn!("wrapctl paste failed, falling back to ydotool: {}", err);
                copy_to_clipboard(text)?;
                return send_ydotool_paste_shortcut().await;
            }
            Err(err) => return Err(err),
        }
    }

    #[cfg(target_os = "macos")]
    {
        copy_to_clipboard(text)?;
        send_macos_paste_shortcut().await
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = text;
        Err(anyhow::anyhow!(
            "Paste shortcut not supported on this platform"
        ))
    }
}

#[cfg(target_os = "linux")]
async fn paste_transcription_with_wrapctl(text: &str) -> anyhow::Result<()> {
    let mut child = tokio::process::Command::new(wrapctl_bin())
        .arg("paste-stdin")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| anyhow::anyhow!("Failed to start wrapctl: {err}"))?;

    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| anyhow::anyhow!("Failed to open wrapctl stdin"))?;
    stdin.write_all(text.as_bytes()).await?;
    drop(stdin);

    let output = child.wait_with_output().await?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        if stderr.is_empty() {
            return Err(anyhow::anyhow!(
                "wrapctl paste-stdin exited with status {}",
                output.status
            ));
        }
        return Err(anyhow::anyhow!("wrapctl paste-stdin failed: {stderr}"));
    }

    Ok(())
}

#[cfg(target_os = "linux")]
async fn send_ydotool_paste_shortcut() -> anyhow::Result<()> {
    let status = tokio::process::Command::new(ydotool_bin())
        .args(["key", "29:1", "42:1", "47:1", "47:0", "42:0", "29:0"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .map_err(|err| anyhow::anyhow!("Failed to run ydotool: {err}"))?;
    if !status.success() {
        return Err(anyhow::anyhow!("ydotool exited with status {status}"));
    }

    Ok(())
}

#[cfg(target_os = "macos")]
async fn send_macos_paste_shortcut() -> anyhow::Result<()> {
    let status = tokio::process::Command::new("osascript")
        .args([
            "-e",
            "tell application \"System Events\" to keystroke \"v\" using command down",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .map_err(|err| anyhow::anyhow!("Failed to run osascript: {err}"))?;
    if !status.success() {
        return Err(anyhow::anyhow!("osascript exited with status {status}"));
    }

    Ok(())
}

fn resolve_output_mode(
    override_mode: Option<remote::RemoteOutputMode>,
) -> remote::RemoteOutputMode {
    override_mode.unwrap_or_else(read_output_mode)
}

fn read_output_mode() -> remote::RemoteOutputMode {
    let raw = std::env::var("OSTT_REMOTE_OUTPUT_MODE").unwrap_or_else(|_| "paste".to_string());
    match raw.trim().to_ascii_lowercase().as_str() {
        "type" | "typed" | "manual" => remote::RemoteOutputMode::Type,
        _ => remote::RemoteOutputMode::Paste,
    }
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

fn read_stream_duration_ms() -> u64 {
    std::env::var("OSTT_REMOTE_STREAM_MS")
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .map(|value| value.clamp(50, 5000))
        .unwrap_or(300)
}

fn paste_header_label() -> String {
    #[cfg(target_os = "macos")]
    {
        "pasted via cmd+v".to_string()
    }
    #[cfg(target_os = "linux")]
    {
        "pasted via wrapd".to_string()
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        "pasted".to_string()
    }
}

fn type_header_label() -> String {
    "typing via ydotool".to_string()
}

fn ydotool_bin() -> String {
    std::env::var("OSTT_YDOTOOL_BIN")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "ydotool".to_string())
}

fn wrapctl_bin() -> String {
    std::env::var("OSTT_WRAPCTL_BIN")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "wrapctl".to_string())
}

fn allow_ydotool_fallback() -> bool {
    std::env::var("OSTT_ALLOW_YDOTOOL_FALLBACK").is_ok_and(|value| {
        matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        )
    })
}

fn contextualize_bin() -> String {
    std::env::var("OSTT_CONTEXTUALIZE_BIN")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| CONTEXTUALIZE_BIN.to_string())
}

fn read_transcription_timeout() -> Duration {
    std::env::var("OSTT_CONTEXTUALIZE_TIMEOUT_SECONDS")
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .filter(|value| *value > 0)
        .map(Duration::from_secs)
        .unwrap_or(DEFAULT_TRANSCRIPTION_TIMEOUT)
}

#[cfg(test)]
mod tests {
    use super::{
        build_contextualize_args, prune_recording_cache, run_transcription_command,
        CONTEXTUALIZE_CACHE_TTL, RECORDING_CACHE_RETENTION,
    };
    use std::ffi::OsString;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    fn stringify(args: &[OsString]) -> Vec<String> {
        args.iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect()
    }

    fn temp_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "ostt-{}-{}-{}",
            label,
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_else(|_| Duration::from_secs(0))
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn build_contextualize_args_includes_keywords_file() {
        let args = build_contextualize_args(
            Path::new("/tmp/audio.mp3"),
            Some(Path::new("/tmp/keywords.txt")),
        );

        assert_eq!(
            stringify(&args),
            vec![
                "--verbose",
                "cat",
                "-f",
                "raw",
                "--cache-ttl",
                CONTEXTUALIZE_CACHE_TTL,
                "--transcribe-prompt-file",
                "/tmp/keywords.txt",
                "/tmp/audio.mp3",
            ]
        );
    }

    #[test]
    fn build_contextualize_args_omits_keywords_file() {
        let args = build_contextualize_args(Path::new("/tmp/audio.mp3"), None);

        assert_eq!(
            stringify(&args),
            vec![
                "--verbose",
                "cat",
                "-f",
                "raw",
                "--cache-ttl",
                CONTEXTUALIZE_CACHE_TTL,
                "/tmp/audio.mp3",
            ]
        );
    }

    #[test]
    fn prune_recording_cache_removes_only_expired_files() {
        let dir = temp_dir("cache-prune");
        let stale = dir.join("stale.mp3");
        let fresh = dir.join("fresh.mp3");

        fs::write(&stale, "old").unwrap();
        let stale_modified = fs::metadata(&stale).unwrap().modified().unwrap();
        fs::write(&fresh, "new").unwrap();
        let mut fresh_modified = fs::metadata(&fresh).unwrap().modified().unwrap();
        while fresh_modified
            .duration_since(stale_modified)
            .map(|delta| delta.is_zero())
            .unwrap_or(true)
        {
            std::thread::sleep(Duration::from_millis(20));
            fs::write(&fresh, "newer").unwrap();
            fresh_modified = fs::metadata(&fresh).unwrap().modified().unwrap();
        }
        let spacing = fresh_modified.duration_since(stale_modified).unwrap();
        let now = stale_modified + RECORDING_CACHE_RETENTION + spacing / 2;

        prune_recording_cache(&dir, now, RECORDING_CACHE_RETENTION);

        assert!(!stale.exists());
        assert!(fresh.exists());
        fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn run_transcription_command_returns_trimmed_stdout() {
        let args = vec![
            OsString::from("-c"),
            OsString::from("printf ' hello world \\n'"),
        ];

        let transcript = run_transcription_command("sh", &args, Duration::from_secs(1))
            .await
            .unwrap();

        assert_eq!(transcript, "hello world");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn run_transcription_command_rejects_empty_stdout() {
        let args = vec![OsString::from("-c"), OsString::from("printf ''")];
        let err = run_transcription_command("sh", &args, Duration::from_secs(1))
            .await
            .unwrap_err();

        assert!(err.to_string().contains("returned no transcript"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn run_transcription_command_uses_stderr_on_failure() {
        let args = vec![
            OsString::from("-c"),
            OsString::from("printf 'boom' >&2; exit 3"),
        ];
        let err = run_transcription_command("sh", &args, Duration::from_secs(1))
            .await
            .unwrap_err();

        assert!(err.to_string().contains("boom"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn run_transcription_command_times_out_for_stalled_process() {
        let args = vec![OsString::from("-c"), OsString::from("sleep 5")];
        let err = run_transcription_command("sh", &args, Duration::from_millis(100))
            .await
            .unwrap_err();

        assert!(err.to_string().contains("timed out"));
    }

    #[tokio::test]
    async fn run_transcription_command_reports_missing_binary() {
        let args: Vec<OsString> = Vec::new();
        let err = run_transcription_command(
            "ostt-binary-that-does-not-exist",
            &args,
            Duration::from_secs(1),
        )
        .await
        .unwrap_err();

        assert!(err.to_string().contains("Could not find"));
    }
}
