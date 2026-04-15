//! Audio recording and format conversion module.
//!
//! This module handles audio input device management, PCM sample capture, and
//! format conversion using ffmpeg. Audio is captured from the system's default
//! input device, converted to mono, and saved in the requested format.

use super::ffmpeg::find_ffmpeg;
use anyhow::{anyhow, Result};
use chrono::Utc;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use hound::WavWriter;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};

#[cfg(target_os = "linux")]
use std::fs::OpenOptions;
#[cfg(target_os = "linux")]
use std::os::unix::io::AsRawFd;

/// Records audio from a specified or default input device.
///
/// Features:
/// - Captures from a specified input device or system default at its native sample rate
/// - Converts multi-channel audio to mono by averaging channels
/// - Saves audio via ffmpeg for format flexibility
/// - Automatic cleanup of temporary files
/// - Pause and resume support
pub struct AudioRecorder {
    /// Actual recording sample rate from device
    sample_rate: u32,
    /// Recorded audio samples (i16 PCM mono)
    samples: Arc<Mutex<Vec<i16>>>,
    /// Active audio input stream (kept alive during recording)
    stream: Option<cpal::Stream>,
    /// Number of channels in device's native format
    device_channels: usize,
    /// Whether recording is currently paused
    is_paused: Arc<Mutex<bool>>,
    /// Device name or "default" to use the system default device
    device_name: String,
}

pub struct RecordedAudio {
    sample_rate: u32,
    samples: Vec<i16>,
}

fn append_transcription_debug_log(message: impl AsRef<str>) {
    let state_dir = if let Ok(xdg_state) = std::env::var("XDG_STATE_HOME") {
        PathBuf::from(xdg_state).join("ostt")
    } else {
        match dirs::home_dir() {
            Some(home) => home.join(".local").join("state").join("ostt"),
            None => return,
        }
    };

    if std::fs::create_dir_all(&state_dir).is_err() {
        return;
    }

    let path = state_dir.join("transcription-latest.log");
    let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    else {
        return;
    };

    let _ = std::io::Write::write_all(
        &mut file,
        format!("{} {}\n", Utc::now().to_rfc3339(), message.as_ref()).as_bytes(),
    );
}

impl AudioRecorder {
    /// Creates a new audio recorder with requested sample rate and device.
    ///
    /// # Arguments
    /// * `requested_sample_rate` - The desired sample rate in Hz (actual may differ based on device)
    /// * `device_name` - Device name/ID to use. Use "default" for system default device
    ///
    /// Note: The actual recording sample rate may differ based on device capabilities.
    /// Call `get_sample_rate()` after `start_recording()` to get the actual rate.
    pub fn new(requested_sample_rate: u32, device_name: String) -> Self {
        Self {
            sample_rate: requested_sample_rate,
            samples: Arc::new(Mutex::new(Vec::new())),
            stream: None,
            device_channels: 1,
            is_paused: Arc::new(Mutex::new(false)),
            device_name,
        }
    }

    /// Starts recording from the configured input device.
    ///
    /// # Errors
    /// - If the specified device is not available
    /// - If device configuration fails
    /// - If audio stream creation fails
    pub fn start_recording(&mut self) -> Result<()> {
        let host = cpal::default_host();

        if self.device_name == "default" {
            let candidates = suppress_alsa_warnings(|| resolve_default_input_devices(&host))?;
            let mut failures = Vec::new();

            for device in candidates {
                let device_name = device
                    .name()
                    .unwrap_or_else(|_| "Unknown device".to_string());
                match self.activate_device(device) {
                    Ok(()) => return Ok(()),
                    Err(err) => {
                        append_transcription_debug_log(format!(
                            "phase=device-open-failed device={} error={}",
                            device_name, err
                        ));
                        failures.push(format!("{device_name}: {err}"));
                    }
                }
            }

            return Err(anyhow!(
                "Failed to open any default input device. Tried: {}",
                failures.join(" | ")
            ));
        }

        let device = suppress_alsa_warnings(|| find_device_by_name(&host, &self.device_name))?;
        self.activate_device(device)
    }

    fn activate_device(&mut self, device: cpal::Device) -> Result<()> {
        let device_name = device
            .name()
            .unwrap_or_else(|_| "Unknown device".to_string());
        tracing::info!("Recording device: {}", device_name);

        let device_config = device.default_input_config()?;
        let device_sample_rate = device_config.sample_rate().0;
        let num_channels = device_config.channels() as usize;

        if device_sample_rate != self.sample_rate {
            tracing::warn!(
                "Requested sample rate {}Hz but device uses {}Hz. Recording at device rate.",
                self.sample_rate,
                device_sample_rate
            );
        }

        tracing::debug!(
            "Device configuration: {}Hz, {} channels",
            device_sample_rate,
            num_channels
        );

        self.sample_rate = device_sample_rate;
        self.device_channels = num_channels;

        let samples_arc = Arc::clone(&self.samples);
        let pause_arc = Arc::clone(&self.is_paused);
        let callback_channels = num_channels;

        let stream = device.build_input_stream(
            &device_config.into(),
            move |data: &[i16], _: &cpal::InputCallbackInfo| {
                let is_paused = *pause_arc.lock().unwrap();
                if !is_paused {
                    Self::handle_audio_callback(data, &samples_arc, callback_channels);
                }
            },
            |err| {
                tracing::error!("Audio stream error: {}", err);
            },
            None,
        )?;

        stream.play()?;
        self.stream = Some(stream);

        append_transcription_debug_log(format!("phase=device-opened device={}", device_name));
        tracing::debug!("Audio stream started");
        Ok(())
    }

    /// Stops recording and saves audio to the specified output file.
    ///
    /// The audio is first saved as a temporary WAV file, then converted to the
    /// requested format using ffmpeg. The temporary file is cleaned up after conversion.
    ///
    /// # Arguments
    /// * `output_path` - Path where the final encoded audio will be saved
    /// * `format` - ffmpeg codec and options, e.g., "mp3 -ab 16k -ar 12000"
    ///
    /// # Errors
    /// - If no samples were recorded
    /// - If temporary WAV creation fails
    /// - If ffmpeg conversion fails
    pub fn stop_recording(&mut self, output_path: Option<PathBuf>, format: &str) -> Result<()> {
        let recording = self.finish_recording()?;
        recording.save(output_path, format)
    }

    pub fn finish_recording(&mut self) -> Result<RecordedAudio> {
        append_transcription_debug_log("phase=finish-recording-start");
        // Stop the audio stream
        self.stream = None;

        let samples = self.samples.lock().unwrap().clone();
        let sample_count = samples.len();

        if sample_count == 0 {
            tracing::warn!("Recording stopped with no samples captured");
            return Ok(RecordedAudio {
                sample_rate: self.sample_rate,
                samples,
            });
        }

        // Calculate and log recording duration
        let duration_secs = sample_count as f32 / self.sample_rate as f32;
        tracing::info!(
            "Recording stopped: {:.2}s ({} samples at {}Hz)",
            duration_secs,
            sample_count,
            self.sample_rate
        );

        append_transcription_debug_log(format!(
            "phase=finish-recording-done sample_rate={} sample_count={}",
            self.sample_rate, sample_count
        ));

        Ok(RecordedAudio {
            sample_rate: self.sample_rate,
            samples,
        })
    }

    /// Handles incoming audio data from the audio callback.
    ///
    /// Converts multi-channel audio to mono by averaging all channels.
    fn handle_audio_callback(
        data: &[i16],
        samples_arc: &Arc<Mutex<Vec<i16>>>,
        num_channels: usize,
    ) {
        let mut samples = samples_arc.lock().unwrap();

        match num_channels {
            1 => {
                // Mono: use samples directly
                samples.extend_from_slice(data);
            }
            2 => {
                // Stereo: average pairs of samples
                for chunk in data.chunks_exact(2) {
                    let left = chunk[0] as i32;
                    let right = chunk[1] as i32;
                    let mono = ((left + right) / 2) as i16;
                    samples.push(mono);
                }
            }
            _ => {
                // Multi-channel: average all channels per sample
                for chunk in data.chunks_exact(num_channels) {
                    let sum: i32 = chunk.iter().map(|&s| s as i32).sum();
                    let mono = (sum / num_channels as i32) as i16;
                    samples.push(mono);
                }
            }
        }
    }

    // Getters for recorded data

    /// Returns a clone of all recorded samples.
    pub fn samples(&self) -> Vec<i16> {
        self.samples.lock().unwrap().clone()
    }

    /// Returns the number of recorded samples.
    pub fn sample_count(&self) -> usize {
        self.samples.lock().unwrap().len()
    }

    /// Returns the actual sample rate of the recording.
    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    /// Pauses recording without stopping the audio stream or losing samples.
    pub fn pause(&self) {
        *self.is_paused.lock().unwrap() = true;
        tracing::debug!("Recording paused");
    }

    /// Resumes recording from a paused state.
    pub fn resume(&self) {
        *self.is_paused.lock().unwrap() = false;
        tracing::debug!("Recording resumed");
    }

    /// Returns whether recording is currently paused.
    pub fn is_paused(&self) -> bool {
        *self.is_paused.lock().unwrap()
    }

    /// Toggles between paused and recording states.
    pub fn toggle_pause(&self) {
        let mut paused = self.is_paused.lock().unwrap();
        *paused = !*paused;
        if *paused {
            tracing::debug!("Recording paused");
        } else {
            tracing::debug!("Recording resumed");
        }
    }
}

impl RecordedAudio {
    pub fn save(&self, output_path: Option<PathBuf>, format: &str) -> Result<()> {
        if let Some(output_file) = output_path {
            let temp_wav = create_temp_wav_path();

            append_transcription_debug_log(format!(
                "phase=save-wav-start temp_wav={} samples={}",
                temp_wav.display(),
                self.samples.len()
            ));
            save_wav(self.sample_rate, &self.samples, &temp_wav)?;
            append_transcription_debug_log(format!(
                "phase=save-wav-done temp_wav={}",
                temp_wav.display()
            ));
            append_transcription_debug_log(format!(
                "phase=ffmpeg-start output={} format={}",
                output_file.display(),
                format
            ));
            convert_with_ffmpeg(&temp_wav, &output_file, format)?;
            append_transcription_debug_log(format!(
                "phase=ffmpeg-done output={}",
                output_file.display()
            ));

            if let Err(err) = std::fs::remove_file(&temp_wav) {
                tracing::debug!("Failed to remove temp file: {}", err);
            }

            let file_size = std::fs::metadata(&output_file)?.len();
            tracing::info!(
                "Audio saved: {} ({} bytes, format: {})",
                output_file.display(),
                file_size,
                format
            );
        }

        Ok(())
    }
}

// Maintain backward compatibility with existing API
impl AudioRecorder {
    /// Deprecated: Use `samples()` instead.
    pub fn get_samples(&self) -> Vec<i16> {
        self.samples()
    }

    /// Deprecated: Use `sample_rate()` instead.
    pub fn get_sample_rate(&self) -> u32 {
        self.sample_rate()
    }
}

fn save_wav(sample_rate: u32, samples: &[i16], path: &Path) -> Result<()> {
    let wav_spec = hound::WavSpec {
        channels: 1,
        sample_rate,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };

    let mut writer = WavWriter::create(path, wav_spec)?;

    for &sample in samples {
        writer.write_sample(sample)?;
    }

    writer.finalize()?;
    tracing::debug!("Temporary WAV created: {}", path.display());
    Ok(())
}

fn convert_with_ffmpeg(input_wav: &Path, output_path: &Path, format: &str) -> Result<()> {
    let format_parts: Vec<&str> = format.split_whitespace().collect();

    if format_parts.is_empty() {
        return Err(anyhow!("Invalid format string: empty"));
    }

    let codec = format_parts[0];
    let ffmpeg_path = find_ffmpeg()?;

    let mut cmd = Command::new(&ffmpeg_path);
    cmd.arg("-loglevel")
        .arg("error")
        .arg("-i")
        .arg(input_wav)
        .arg("-acodec")
        .arg(codec)
        .arg("-ac")
        .arg("1")
        .arg("-y");

    for option in &format_parts[1..] {
        cmd.arg(option);
    }

    cmd.arg(output_path);

    let output = cmd.output()?;

    if output.status.success() {
        tracing::debug!("Audio converted to {} format", codec);
        Ok(())
    } else {
        let error_msg = String::from_utf8_lossy(&output.stderr);
        tracing::error!("ffmpeg conversion failed: {}", error_msg);
        Err(anyhow!("Audio encoding failed: {error_msg}"))
    }
}

fn create_temp_wav_path() -> PathBuf {
    std::env::temp_dir().join(format!("ostt_{}.wav", std::process::id()))
}

fn resolve_default_input_devices(host: &cpal::Host) -> Result<Vec<cpal::Device>> {
    let mut candidates = Vec::new();
    let mut seen = std::collections::HashSet::new();

    let mut push_unique = |device: cpal::Device| {
        let key = device
            .name()
            .unwrap_or_else(|_| format!("unknown-{}", seen.len()));
        if seen.insert(key) {
            candidates.push(device);
        }
    };

    #[cfg(target_os = "linux")]
    {
        if let Some(device) = resolve_linux_session_default_input_device(host)? {
            push_unique(device);
        }
    }

    if let Some(device) = host.default_input_device() {
        push_unique(device);
    }

    let devices = host
        .input_devices()
        .map_err(|e| anyhow!("Failed to enumerate devices: {e}"))?;
    for device in devices {
        push_unique(device);
    }

    if candidates.is_empty() {
        return Err(anyhow!("No audio input device available"));
    }

    Ok(candidates)
}

/// Finds an audio input device by name or numeric index.
///
/// # Arguments
/// * `host` - The cpal audio host
/// * `device_spec` - Either "default" for system default, a device name, or a numeric index (0, 1, 2, etc.)
///
/// # Errors
/// - If no device with the specified name/index is found
fn find_device_by_name(host: &cpal::Host, device_spec: &str) -> Result<cpal::Device> {
    // Try to parse as a numeric index first
    if let Ok(index) = device_spec.parse::<usize>() {
        let devices: Vec<_> = host
            .input_devices()
            .map_err(|e| anyhow!("Failed to enumerate devices: {e}"))?
            .collect();

        if index < devices.len() {
            return Ok(devices.into_iter().nth(index).unwrap());
        } else {
            return Err(anyhow!(
                "Device index {} is out of range (0-{})",
                index,
                devices.len().saturating_sub(1)
            ));
        }
    }

    // Try to find by name
    let devices = host
        .input_devices()
        .map_err(|e| anyhow!("Failed to enumerate devices: {e}"))?;

    for device in devices {
        if let Ok(name) = device.name() {
            if name == device_spec {
                return Ok(device);
            }
        }
    }

    Err(anyhow!(
        "Audio input device '{}' not found. Use 'ostt list-devices' to see available devices.",
        device_spec
    ))
}

fn find_device_by_partial_name(host: &cpal::Host, needle: &str) -> Result<Option<cpal::Device>> {
    let needle_lower = needle.to_lowercase();
    if needle_lower.is_empty() {
        return Ok(None);
    }

    let devices = host
        .input_devices()
        .map_err(|e| anyhow!("Failed to enumerate devices: {e}"))?;

    let mut best_match: Option<(u8, cpal::Device, String)> = None;

    for device in devices {
        let Ok(name) = device.name() else {
            continue;
        };

        let name_lower = name.to_lowercase();
        let score = if name_lower == needle_lower {
            4
        } else if name_lower.starts_with(&needle_lower) || needle_lower.starts_with(&name_lower) {
            3
        } else if name_lower.contains(&needle_lower) || needle_lower.contains(&name_lower) {
            2
        } else {
            0
        };

        if score == 0 {
            continue;
        }

        let should_replace = best_match
            .as_ref()
            .map(|(best_score, _, _)| score > *best_score)
            .unwrap_or(true);

        if should_replace {
            best_match = Some((score, device, name));
        }
    }

    if let Some((_, device, matched_name)) = best_match {
        tracing::info!(
            "Matched requested input '{}' to available device '{}'",
            needle,
            matched_name
        );
        return Ok(Some(device));
    }

    Ok(None)
}

#[cfg(target_os = "linux")]
fn resolve_linux_session_default_input_device(host: &cpal::Host) -> Result<Option<cpal::Device>> {
    if let Some(source_name) = linux_default_source_name_from_wpctl() {
        if let Some(device) = find_device_by_partial_name(host, &source_name)? {
            tracing::info!(
                "Using Linux session default input source from wpctl: '{}'",
                source_name
            );
            return Ok(Some(device));
        }
    }

    if let Some((node_name, description)) = linux_default_source_from_pactl() {
        let mut hints = Vec::new();

        if let Some(desc) = description {
            hints.push(strip_pulse_profile_suffix(&desc));
        }

        hints.push(node_name_to_hint(&node_name));

        for hint in hints {
            if hint.is_empty() {
                continue;
            }

            if let Some(device) = find_device_by_partial_name(host, &hint)? {
                tracing::info!(
                    "Using Linux session default input source from pactl: '{}'",
                    hint
                );
                return Ok(Some(device));
            }
        }
    }

    Ok(None)
}

#[cfg(target_os = "linux")]
fn linux_default_source_name_from_wpctl() -> Option<String> {
    let output = command_stdout("wpctl", &["inspect", "@DEFAULT_AUDIO_SOURCE@"])?;

    for line in output.lines() {
        let trimmed = line.trim();
        if let Some((_, value)) = trimmed.split_once("node.nick = \"") {
            return value.strip_suffix('"').map(|s| s.to_string());
        }
        if let Some((_, value)) = trimmed.split_once("node.description = \"") {
            return value.strip_suffix('"').map(strip_pulse_profile_suffix);
        }
    }

    None
}

#[cfg(target_os = "linux")]
fn linux_default_source_from_pactl() -> Option<(String, Option<String>)> {
    let info = command_stdout("pactl", &["info"])?;
    let default_source = info
        .lines()
        .find_map(|line| line.strip_prefix("Default Source: "))
        .map(str::trim)?
        .to_string();

    let sources = command_stdout("pactl", &["list", "sources"])?;
    let mut current_name: Option<String> = None;
    let mut description: Option<String> = None;

    for line in sources.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("Source #") {
            if current_name.as_deref() == Some(default_source.as_str()) {
                return Some((default_source, description));
            }
            current_name = None;
            description = None;
            continue;
        }

        if let Some(value) = trimmed.strip_prefix("Name: ") {
            current_name = Some(value.trim().to_string());
            continue;
        }

        if let Some(value) = trimmed.strip_prefix("Description: ") {
            if current_name.as_deref() == Some(default_source.as_str()) {
                description = Some(value.trim().to_string());
            }
        }
    }

    Some((default_source, description))
}

#[cfg(target_os = "linux")]
fn strip_pulse_profile_suffix(label: &str) -> String {
    const SUFFIXES: &[&str] = &[
        " Analog Stereo",
        " Analog Mono",
        " Digital Stereo",
        " Stereo",
        " Mono",
    ];

    let trimmed = label.trim();
    for suffix in SUFFIXES {
        if let Some(base) = trimmed.strip_suffix(suffix) {
            return base.trim().to_string();
        }
    }
    trimmed.to_string()
}

#[cfg(target_os = "linux")]
fn node_name_to_hint(node_name: &str) -> String {
    let after_prefix = node_name.strip_prefix("alsa_input.").unwrap_or(node_name);
    let core = after_prefix.split('.').next().unwrap_or(after_prefix);
    let core = core.strip_prefix("usb-").unwrap_or(core);

    let mut parts: Vec<&str> = core.split('_').collect();
    if let Some(last) = parts.last() {
        if last
            .chars()
            .all(|c| c.is_ascii_hexdigit() || c.is_ascii_digit() || c == '-')
        {
            parts.pop();
        }
    }

    parts
        .join(" ")
        .replace('-', " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(target_os = "linux")]
fn command_stdout(command: &str, args: &[&str]) -> Option<String> {
    let output = Command::new(command).args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout).ok()
}

/// Temporarily redirects stderr to /dev/null to suppress ALSA library warnings on Linux.
/// On non-Linux platforms, this is a no-op since ALSA doesn't exist.
#[cfg(target_os = "linux")]
fn suppress_alsa_warnings<F, T>(f: F) -> Result<T>
where
    F: FnOnce() -> Result<T>,
{
    // Open /dev/null for writing
    let dev_null = OpenOptions::new()
        .write(true)
        .open("/dev/null")
        .map_err(|e| anyhow!("Failed to open /dev/null: {e}"))?;

    let dev_null_fd = dev_null.as_raw_fd();

    // Save the current stderr file descriptor
    let old_stderr = unsafe { libc::dup(libc::STDERR_FILENO) };
    if old_stderr == -1 {
        return Err(anyhow!("Failed to duplicate stderr"));
    }

    // Redirect stderr to /dev/null
    let redirect_result = unsafe { libc::dup2(dev_null_fd, libc::STDERR_FILENO) };
    if redirect_result == -1 {
        unsafe { libc::close(old_stderr) };
        return Err(anyhow!("Failed to redirect stderr"));
    }

    // Execute the closure
    let result = f();

    // Restore the original stderr
    unsafe {
        libc::dup2(old_stderr, libc::STDERR_FILENO);
        libc::close(old_stderr);
    }

    result
}

/// On non-Linux platforms, no stderr suppression is needed since ALSA doesn't exist.
#[cfg(not(target_os = "linux"))]
fn suppress_alsa_warnings<F, T>(f: F) -> Result<T>
where
    F: FnOnce() -> Result<T>,
{
    f()
}
