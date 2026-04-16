//! Terminal user interface for audio recording with configurable visualization.
//!
//! Supports frequency spectrum and time-domain waveform visualization modes.
//! Handles real-time display updates, volume metering, and user input during recording.

use crossterm::{
    event::{self, Event, KeyCode},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode},
};
use ratatui::{
    prelude::*,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Gauge, Paragraph, Sparkline},
};
use std::error::Error;
use std::io::{stdout, Stdout};

use crate::config::VisualizationType;

use super::visualizations::{resize_waveform, update_waveform, SpectrumAnalyzer};
use super::TranscriptionAnimation;

#[derive(Debug, Clone, Copy)]
struct RgbColor {
    r: u8,
    g: u8,
    b: u8,
}

impl RgbColor {
    fn new(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b }
    }

    fn to_color(self) -> Color {
        Color::Rgb(self.r, self.g, self.b)
    }

    fn lerp(self, other: Self, t: f32) -> Self {
        let clamp = |value: f32| value.clamp(0.0, 255.0).round() as u8;
        Self {
            r: clamp(self.r as f32 + (other.r as f32 - self.r as f32) * t),
            g: clamp(self.g as f32 + (other.g as f32 - self.g as f32) * t),
            b: clamp(self.b as f32 + (other.b as f32 - self.b as f32) * t),
        }
    }

    fn greyed(self, mix: f32) -> Self {
        let gray_value = ((self.r as u16 + self.g as u16 + self.b as u16) / 3) as u8;
        let gray = Self::new(gray_value, gray_value, gray_value);
        self.lerp(gray, mix.clamp(0.0, 1.0))
    }
}

#[derive(Debug, Clone, Copy)]
struct WaveformColors {
    top_fg: RgbColor,
    top_bg: RgbColor,
    bottom_fg: RgbColor,
    bottom_bg: RgbColor,
    footer_fg: RgbColor,
    footer_bg: RgbColor,
    border_fg: RgbColor,
}

impl WaveformColors {
    fn base() -> Self {
        Self {
            top_fg: RgbColor::new(206, 224, 220),
            top_bg: RgbColor::new(0, 0, 0),
            bottom_fg: RgbColor::new(0, 0, 0),
            bottom_bg: RgbColor::new(185, 207, 212),
            footer_fg: RgbColor::new(185, 207, 212),
            footer_bg: RgbColor::new(0, 0, 0),
            border_fg: RgbColor::new(80, 90, 95),
        }
    }

    fn cancel_target() -> Self {
        let base = Self::base();
        let neutral = RgbColor::new(120, 130, 135);
        Self {
            top_fg: neutral,
            top_bg: base.top_bg,
            bottom_fg: neutral,
            bottom_bg: base.bottom_bg,
            footer_fg: base.footer_fg.greyed(0.6),
            footer_bg: base.footer_bg,
            border_fg: base.border_fg.greyed(0.4),
        }
    }
}

/// User input command during recording.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordingCommand {
    /// Continue recording (no key pressed)
    Continue,
    /// Proceed to transcription (Enter key)
    Transcribe,
    /// Exit without transcription (Escape or 'q')
    Cancel,
    /// Pause/resume recording (Space key)
    TogglePause,
}

/// Terminal UI for audio recording with configurable visualization.
///
/// Supports multiple visualization types: frequency spectrum or time-domain waveform.
/// Displays real-time visualization, volume levels, recording duration, and animated transcription progress.
pub struct OsttTui {
    terminal: Terminal<CrosstermBackend<Stdout>>,
    display_data: Vec<u64>,
    last_sample_time: std::time::Instant,
    sample_interval: std::time::Duration,
    last_peak: u8,
    terminal_width: usize,
    sample_rate: u32,
    recording_start_time: std::time::Instant,
    peak_hold: u8,
    peak_hold_time: std::time::Instant,
    peak_volume_threshold: u8,
    reference_level_db: i8,
    /// Whether recording is currently paused
    pub is_paused: bool,
    /// Total time paused (accumulated when paused)
    pause_duration: std::time::Duration,
    /// When pause started (for calculating pause duration)
    pause_start_time: Option<std::time::Instant>,
    /// Visualization type (spectrum or waveform)
    visualization_type: VisualizationType,
    /// Spectrum analyzer (used when visualization_type is Spectrum)
    spectrum_analyzer: Option<SpectrumAnalyzer>,
    /// Cancel animation start time
    cancel_animation_start: Option<std::time::Instant>,
    /// Cancel animation duration
    cancel_animation_duration: std::time::Duration,
    /// Snapshot of recording duration when cancel begins
    cancel_duration_snapshot: Option<std::time::Duration>,
}

impl OsttTui {
    /// Creates a new TUI instance and enters alternate screen mode.
    ///
    /// # Errors
    /// - If terminal cannot be initialized
    /// - If raw mode cannot be enabled
    /// - If alternate screen cannot be entered
    pub fn new(
        sample_rate: u32,
        peak_volume_threshold: u8,
        reference_level_db: i8,
        visualization_type: VisualizationType,
    ) -> Result<Self, Box<dyn Error>> {
        enable_raw_mode()?;
        let mut stdout = stdout();
        execute!(stdout, crossterm::terminal::EnterAlternateScreen)?;

        let backend = CrosstermBackend::new(stdout);
        let terminal = Terminal::new(backend)?;

        let size = terminal.size()?;
        let terminal_width = size.width as usize;

        let sample_interval = std::time::Duration::from_millis(50);

        // Initialize visualization-specific data
        let display_data = vec![0u64; terminal_width];
        let spectrum_analyzer = if visualization_type == VisualizationType::Spectrum {
            Some(SpectrumAnalyzer::new(terminal_width))
        } else {
            None
        };

        let now = std::time::Instant::now();
        Ok(OsttTui {
            terminal,
            display_data,
            last_sample_time: now,
            sample_interval,
            last_peak: 0,
            terminal_width,
            sample_rate,
            recording_start_time: now,
            peak_hold: 0,
            peak_hold_time: now,
            peak_volume_threshold,
            reference_level_db,
            is_paused: false,
            pause_duration: std::time::Duration::ZERO,
            pause_start_time: None,
            visualization_type,
            spectrum_analyzer,
            cancel_animation_start: None,
            cancel_animation_duration: std::time::Duration::from_millis(320),
            cancel_duration_snapshot: None,
        })
    }

    /// Renders the visualization with current volume and recording duration.
    ///
    /// # Errors
    /// - If terminal rendering fails
    pub fn render_waveform(&mut self, samples: &[i16]) -> Result<(), Box<dyn Error>> {
        let cancel_progress = self.cancel_progress().unwrap_or(0.0).clamp(0.0, 1.0);
        let cancel_active = cancel_progress > 0.0;
        let current_volume = if cancel_active {
            self.last_peak
        } else {
            self.calculate_volume(samples)
        };
        let base_colors = WaveformColors::base();
        let target_colors = WaveformColors::cancel_target();
        let waveform_colors = if cancel_progress > 0.0 {
            let eased = 1.0 - (1.0 - cancel_progress).powf(2.0);
            let cancel_wave_color = base_colors.top_fg.lerp(target_colors.bottom_fg, eased);
            WaveformColors {
                top_fg: cancel_wave_color,
                bottom_fg: cancel_wave_color,
                top_bg: base_colors.top_bg,
                bottom_bg: base_colors.bottom_bg,
                footer_fg: base_colors.footer_fg,
                footer_bg: base_colors.footer_bg,
                border_fg: base_colors.border_fg,
            }
        } else {
            base_colors
        };
        let footer_colors = base_colors;
        let footer_inactive_color = Color::Rgb(120, 130, 135);

        if !self.is_paused
            && !cancel_active
            && self.last_sample_time.elapsed() >= self.sample_interval
        {
            match self.visualization_type {
                VisualizationType::Spectrum => {
                    if let Some(analyzer) = &mut self.spectrum_analyzer {
                        analyzer.update(samples, self.sample_rate, self.reference_level_db);
                        self.display_data = analyzer.data().to_vec();
                    }
                }
                VisualizationType::Waveform => {
                    update_waveform(&mut self.display_data, current_volume, self.terminal_width);
                }
            }

            self.last_sample_time = std::time::Instant::now();
        }

        let size = self.terminal.size()?;
        let current_width = size.width as usize;

        if current_width != self.terminal_width {
            self.terminal_width = current_width;

            match self.visualization_type {
                VisualizationType::Spectrum => {
                    if let Some(analyzer) = &mut self.spectrum_analyzer {
                        analyzer.resize(
                            current_width,
                            samples,
                            self.sample_rate,
                            self.reference_level_db,
                        );
                        self.display_data = analyzer.data().to_vec();
                    }
                }
                VisualizationType::Waveform => {
                    resize_waveform(&mut self.display_data, self.terminal_width);
                }
            }
        }

        let inverted_data: Vec<u64> = self
            .display_data
            .iter()
            .map(|&v| 100_u64.saturating_sub(v))
            .collect();
        let render_data;
        let top_data = if cancel_active {
            let scale = (1.0 - cancel_progress).clamp(0.0, 1.0);
            render_data = self
                .display_data
                .iter()
                .map(|&v| ((v as f32) * scale).round().clamp(0.0, 100.0) as u64)
                .collect::<Vec<_>>();
            &render_data
        } else {
            &self.display_data
        };

        // Pre-calculate values to avoid borrow checker issues in closure
        let is_paused = self.is_paused;
        let peak_hold = self.peak_hold;
        let last_peak = self.last_peak;
        let peak_volume_threshold = self.peak_volume_threshold;
        let recording_duration = if cancel_active {
            self.cancel_duration_snapshot
                .unwrap_or_else(|| self.get_recording_duration())
        } else {
            self.get_recording_duration()
        };

        self.terminal.draw(|frame| {
            let area = frame.area();

            let footer_height = 1;

            let content_area = Rect {
                x: area.x,
                y: area.y,
                width: area.width,
                height: area.height.saturating_sub(footer_height),
            };

            let top_area_height = content_area.height / 3 * 2;
            let bottom_area_height = content_area.height.saturating_sub(top_area_height);

            let top_area = Rect {
                x: content_area.x,
                y: content_area.y,
                width: content_area.width,
                height: top_area_height,
            };

            let bottom_area = Rect {
                x: content_area.x,
                y: content_area.y + top_area_height,
                width: content_area.width,
                height: bottom_area_height,
            };

            if cancel_active {
                for y in top_area.y..top_area.y + top_area.height {
                    for x in top_area.x..top_area.x + top_area.width {
                        frame.buffer_mut().set_string(
                            x,
                            y,
                            " ",
                            Style::default().bg(base_colors.top_bg.to_color()),
                        );
                    }
                }

                for y in bottom_area.y..bottom_area.y + bottom_area.height {
                    for x in bottom_area.x..bottom_area.x + bottom_area.width {
                        frame.buffer_mut().set_string(
                            x,
                            y,
                            " ",
                            Style::default().bg(base_colors.top_bg.to_color()),
                        );
                    }
                }

                let top_style = Style::default()
                    .fg(waveform_colors.top_fg.to_color())
                    .bg(base_colors.top_bg.to_color());
                let bottom_style = Style::default()
                    .fg(waveform_colors.bottom_fg.to_color())
                    .bg(base_colors.top_bg.to_color());

                let center_y = bottom_area.y;
                let top_max = top_area.height as f32;
                let bottom_max = bottom_area.height as f32;

                for (idx, value) in top_data.iter().enumerate() {
                    let x = match u16::try_from(idx) {
                        Ok(col) => content_area.x + col,
                        Err(_) => break,
                    };
                    if x >= content_area.x + content_area.width {
                        break;
                    }

                    let normalized = (*value as f32 / 100.0).clamp(0.0, 1.0);
                    let top_height = (normalized * top_max).round() as i16;
                    let bottom_height = (normalized * bottom_max).round() as i16;

                    for step in 0..top_height {
                        let y = center_y.saturating_sub(1 + step as u16);
                        frame.buffer_mut().set_string(x, y, "█", top_style);
                    }

                    for step in 0..bottom_height {
                        let y = center_y + step as u16;
                        if y >= bottom_area.y + bottom_area.height {
                            break;
                        }
                        frame.buffer_mut().set_string(x, y, "█", bottom_style);
                    }
                }
            } else {
                let top_sparkline = Sparkline::default().data(top_data).max(100).style(
                    Style::default()
                        .bg(base_colors.top_bg.to_color())
                        .fg(waveform_colors.top_fg.to_color()),
                );

                frame.render_widget(top_sparkline, top_area);

                let bottom_sparkline = Sparkline::default().data(&inverted_data).max(100).style(
                    Style::default()
                        .bg(base_colors.bottom_bg.to_color())
                        .fg(waveform_colors.bottom_fg.to_color()),
                );

                frame.render_widget(bottom_sparkline, bottom_area);
            }

            let footer_area = Rect {
                x: area.x,
                y: area.y + area.height.saturating_sub(footer_height),
                width: area.width,
                height: footer_height,
            };

            // When paused, show zeros for meters
            let (display_peak, display_volume) = if is_paused {
                (0u8, 0u8)
            } else {
                (peak_hold, last_peak)
            };

            let peak_style = if cancel_active {
                Style::default().fg(footer_inactive_color)
            } else if display_peak >= peak_volume_threshold {
                Style::default()
                    .bg(Color::Red)
                    .fg(Color::Rgb(255, 255, 255))
            } else {
                Style::default()
            };

            let duration_secs = recording_duration.as_secs();
            let minutes = duration_secs / 60;
            let secs = duration_secs % 60;
            let duration_span = ratatui::text::Span::raw(format!("{minutes}:{secs:02}"));

            let peak_span = ratatui::text::Span::styled(format!("{display_peak}%"), peak_style);

            let vol_span = ratatui::text::Span::raw(format!("{display_volume}%"));

            // Show pause symbol instead of red dot when paused
            let dev_mode = std::env::var("OSTT_DEV").is_ok_and(|value| value == "1");
            let dev_style = Style::default().fg(Color::Green);
            let indicator = if dev_mode {
                ratatui::text::Span::styled("◆ ", dev_style)
            } else if is_paused {
                ratatui::text::Span::styled("⏸ ", Style::default().fg(Color::Yellow))
            } else if cancel_active {
                ratatui::text::Span::styled("● ", Style::default().fg(footer_inactive_color))
            } else {
                ratatui::text::Span::styled("● ", Style::default().fg(Color::Red))
            };

            let text_style = if cancel_active {
                Style::default().fg(footer_inactive_color)
            } else {
                Style::default().fg(footer_colors.footer_fg.to_color())
            };
            let help_text = ratatui::text::Line::from(vec![
                indicator,
                ratatui::text::Span::styled(duration_span.content.clone(), text_style),
                ratatui::text::Span::styled(" / ", text_style),
                ratatui::text::Span::styled(vol_span.content.clone(), text_style),
                ratatui::text::Span::styled(" / ", text_style),
                peak_span,
            ]);

            let footer = ratatui::widgets::Paragraph::new(help_text).style(
                Style::default()
                    .fg(footer_colors.footer_fg.to_color())
                    .bg(footer_colors.footer_bg.to_color()),
            );

            if dev_mode && footer_area.width > 8 {
                let chunks = Layout::horizontal([
                    Constraint::Min(0),
                    Constraint::Length(3),
                ])
                .split(footer_area);
                frame.render_widget(footer, chunks[0]);
                let dev_label = Paragraph::new("DEV").style(
                    Style::default()
                        .fg(Color::Green)
                        .bg(footer_colors.footer_bg.to_color()),
                );
                frame.render_widget(dev_label, chunks[1]);
            } else {
                frame.render_widget(footer, footer_area);
            }
        })?;

        Ok(())
    }

    /// Calculates current volume in percentage and updates peak hold tracking.
    ///
    /// Converts RMS (Root Mean Square) audio samples to dBFS and normalizes to 0-100% scale
    /// based on the configured reference level. Also tracks the maximum volume seen in the
    /// last 3 seconds for the peak indicator.
    fn calculate_volume(&mut self, samples: &[i16]) -> u8 {
        if samples.is_empty() {
            return 0;
        }

        let last_samples_count =
            std::cmp::min(self.sample_rate / 20, samples.len() as u32) as usize;
        let recent_samples = &samples[samples.len() - last_samples_count..];

        let sum_of_squares: i64 = recent_samples.iter().map(|&x| (x as i64).pow(2)).sum();
        let mean_square = sum_of_squares / recent_samples.len() as i64;
        let rms = (mean_square as f32).sqrt();

        let db_fs = if rms > 0.0 {
            20.0 * (rms / 32767.0).log10()
        } else {
            -160.0
        };

        let min_db = self.reference_level_db as f32 - 40.0;
        let normalized = ((db_fs - min_db) / 40.0 * 100.0).clamp(4.0, 100.0) as u8;

        self.last_peak = normalized;

        if normalized > self.peak_hold || self.peak_hold_time.elapsed().as_secs() >= 3 {
            self.peak_hold = normalized;
            self.peak_hold_time = std::time::Instant::now();
        }

        normalized
    }

    /// Processes user input and returns the appropriate recording command.
    ///
    /// Only responds to Enter (transcribe), Escape, and 'q' (cancel) keys.
    /// All other keys are ignored.
    ///
    /// # Returns
    /// - `Continue` if no key or unrecognized key was pressed
    /// - `Transcribe` if Enter was pressed
    /// - `Cancel` if Escape or 'q' was pressed
    ///
    /// # Errors
    /// - If event polling fails
    pub fn handle_input(&mut self) -> Result<RecordingCommand, Box<dyn Error>> {
        if event::poll(std::time::Duration::from_millis(50))? {
            if let Event::Key(key) = event::read()? {
                return Ok(match key.code {
                    KeyCode::Enter => {
                        tracing::debug!("Enter pressed: proceeding to transcription");
                        RecordingCommand::Transcribe
                    }
                    KeyCode::Char('q') | KeyCode::Esc => {
                        tracing::debug!("Escape or 'q' pressed: canceling recording");
                        RecordingCommand::Cancel
                    }
                    KeyCode::Char('c')
                        if key
                            .modifiers
                            .contains(crossterm::event::KeyModifiers::CONTROL) =>
                    {
                        tracing::debug!("Ctrl+C pressed: canceling recording");
                        RecordingCommand::Cancel
                    }
                    KeyCode::Char(' ') => {
                        tracing::debug!("Space pressed: toggling pause");
                        self.toggle_pause_state();
                        RecordingCommand::TogglePause
                    }
                    _ => RecordingCommand::Continue,
                });
            }
        }
        Ok(RecordingCommand::Continue)
    }

    /// Handles pause state transitions, managing pause duration tracking.
    fn toggle_pause_state(&mut self) {
        if self.is_paused {
            // Resuming from pause
            if let Some(pause_start) = self.pause_start_time {
                self.pause_duration += pause_start.elapsed();
                self.pause_start_time = None;
            }
            self.is_paused = false;
        } else {
            // Starting pause
            self.pause_start_time = Some(std::time::Instant::now());
            self.is_paused = true;
        }
    }

    /// Gets the elapsed recording time, excluding paused duration.
    fn get_recording_duration(&self) -> std::time::Duration {
        let total_elapsed = self.recording_start_time.elapsed();
        let mut pause_time = self.pause_duration;

        // If currently paused, add the current pause duration
        if self.is_paused {
            if let Some(pause_start) = self.pause_start_time {
                pause_time += pause_start.elapsed();
            }
        }

        total_elapsed.saturating_sub(pause_time)
    }

    /// Renders one frame of the transcription animation.
    ///
    /// # Errors
    /// - If terminal rendering fails
    pub fn render_transcription_animation(
        &mut self,
        animation: &mut TranscriptionAnimation,
    ) -> Result<(), Box<dyn Error>> {
        self.terminal.draw(|f| {
            let main_area = f.area();
            animation.draw(f, main_area);
        })?;
        animation.update();
        Ok(())
    }

    /// Renders the typing progress once transcription has completed.
    ///
    /// # Errors
    /// - If terminal rendering fails
    pub fn render_typing_progress(
        &mut self,
        text: &str,
        typed_count: usize,
        header_label: &str,
    ) -> Result<(), Box<dyn Error>> {
        let total = text.chars().count();
        let typed = typed_count.min(total);
        let ratio = if total == 0 {
            1.0
        } else {
            typed as f64 / total as f64
        };

        let typed_style = Style::default().fg(Color::Rgb(245, 245, 245));
        let untyped_style = Style::default().fg(Color::Rgb(110, 118, 129));
        let cursor_style = Style::default()
            .fg(Color::Rgb(0, 0, 0))
            .bg(Color::Rgb(245, 245, 245))
            .add_modifier(Modifier::BOLD);
        let header_style = Style::default()
            .fg(Color::Rgb(185, 207, 212))
            .bg(Color::Rgb(0, 0, 0));

        self.terminal.draw(|frame| {
            let area = frame.area();

            for y in area.y..area.y + area.height {
                for x in area.x..area.x + area.width {
                    frame.buffer_mut().set_string(
                        x,
                        y,
                        " ",
                        Style::default().bg(Color::Rgb(0, 0, 0)),
                    );
                }
            }

            let show_header = area.height >= 3;
            let show_footer = area.height >= 4;
            let header_height = if show_header { 1 } else { 0 };
            let footer_height = if show_footer { 1 } else { 0 };
            let text_height = area.height.saturating_sub(header_height + footer_height);

            if show_header {
                let percent = (ratio * 100.0).round() as u16;
                let status = format!("{header_label}  {typed}/{total}  {percent}%");
                let header_area = Rect {
                    x: area.x,
                    y: area.y,
                    width: area.width,
                    height: 1,
                };
                let header = Paragraph::new(status)
                    .alignment(Alignment::Center)
                    .style(header_style);
                frame.render_widget(header, header_area);
            }

            let text_area = Rect {
                x: area.x,
                y: area.y + header_height,
                width: area.width,
                height: text_height,
            };

            let use_border = text_area.width > 6 && text_area.height > 4;
            let inner_area = if use_border {
                frame.render_widget(
                    Block::default()
                        .borders(Borders::ALL)
                        .border_style(Style::default().fg(Color::Rgb(80, 90, 95))),
                    text_area,
                );
                Rect {
                    x: text_area.x + 1,
                    y: text_area.y + 1,
                    width: text_area.width.saturating_sub(2),
                    height: text_area.height.saturating_sub(2),
                }
            } else {
                text_area
            };

            if inner_area.width > 0 && inner_area.height > 0 {
                let lines = build_typing_lines(
                    text,
                    typed,
                    inner_area.width as usize,
                    inner_area.height as usize,
                    typed_style,
                    untyped_style,
                    cursor_style,
                );
                let paragraph = Paragraph::new(lines);
                frame.render_widget(paragraph, inner_area);
            }

            if show_footer {
                let footer_area = Rect {
                    x: area.x,
                    y: area.y + area.height.saturating_sub(1),
                    width: area.width,
                    height: 1,
                };
                let gauge = Gauge::default()
                    .gauge_style(
                        Style::default()
                            .fg(Color::Rgb(206, 224, 220))
                            .bg(Color::Rgb(30, 30, 30)),
                    )
                    .ratio(ratio);
                frame.render_widget(gauge, footer_area);
            }
        })?;

        Ok(())
    }

    /// Cleans up terminal state and exits alternate screen mode.
    ///
    /// # Errors
    /// - If terminal mode cannot be disabled
    /// - If cursor cannot be shown
    pub fn cleanup(&mut self) -> Result<(), Box<dyn Error>> {
        disable_raw_mode()?;
        execute!(
            self.terminal.backend_mut(),
            crossterm::terminal::LeaveAlternateScreen
        )?;
        self.terminal.show_cursor()?;
        Ok(())
    }

    /// Starts the cancel animation, transitioning the waveform to a red palette.
    pub fn start_cancel_animation(&mut self) {
        if self.cancel_animation_start.is_none() {
            self.cancel_animation_start = Some(std::time::Instant::now());
            self.cancel_duration_snapshot = Some(self.get_recording_duration());
        }
    }

    /// Returns true if the cancel animation has finished.
    pub fn cancel_animation_done(&self) -> bool {
        self.cancel_progress()
            .is_some_and(|progress| progress >= 1.0)
    }

    fn cancel_progress(&self) -> Option<f32> {
        let start = self.cancel_animation_start?;
        let elapsed = start.elapsed().as_secs_f32();
        let total = self.cancel_animation_duration.as_secs_f32().max(0.001);
        Some((elapsed / total).min(1.0))
    }
}

fn build_typing_lines(
    text: &str,
    typed_count: usize,
    max_width: usize,
    max_height: usize,
    typed_style: Style,
    untyped_style: Style,
    cursor_style: Style,
) -> Vec<Line<'static>> {
    if max_width == 0 || max_height == 0 {
        return Vec::new();
    }

    let chars: Vec<char> = text.chars().collect();
    if chars.is_empty() {
        return vec![Line::from(Span::styled(String::new(), untyped_style))];
    }

    let typed = typed_count.min(chars.len());
    let ranges = wrap_text_ranges(&chars, max_width);
    let total_lines = ranges.len().max(1);

    let cursor_index = if typed < chars.len() {
        Some(typed)
    } else {
        None
    };
    let mut cursor_line = total_lines.saturating_sub(1);
    if let Some(cursor) = cursor_index {
        for (idx, (start, end)) in ranges.iter().enumerate() {
            if (*start <= cursor && cursor < *end) || (*start == *end && cursor == *start) {
                cursor_line = idx;
                break;
            }
        }
    }

    let window_height = max_height.min(total_lines);
    let mut start_line = cursor_line.saturating_sub(window_height / 2);
    let end_line = (start_line + window_height).min(total_lines);
    if end_line - start_line < window_height {
        start_line = end_line.saturating_sub(window_height);
    }

    let mut lines = Vec::new();
    for (start, end) in ranges[start_line..end_line].iter().copied() {
        lines.push(build_line(
            &chars,
            start,
            end,
            typed,
            typed_style,
            untyped_style,
            cursor_style,
        ));
    }

    if lines.len() < max_height {
        let padding = (max_height - lines.len()) / 2;
        for _ in 0..padding {
            lines.insert(0, Line::from(Span::raw("")));
        }
    }

    lines
}

fn build_line(
    chars: &[char],
    start: usize,
    end: usize,
    typed_count: usize,
    typed_style: Style,
    untyped_style: Style,
    cursor_style: Style,
) -> Line<'static> {
    if start >= end {
        return Line::from(Span::raw(""));
    }

    let line_len = end - start;
    let typed_len = typed_count.saturating_sub(start).min(line_len);
    let has_cursor = typed_count < chars.len() && typed_count >= start && typed_count < end;

    let mut spans = Vec::new();
    if typed_len > 0 {
        spans.push(Span::styled(
            chars[start..start + typed_len].iter().collect::<String>(),
            typed_style,
        ));
    }

    if has_cursor {
        let cursor_char = chars[typed_count];
        spans.push(Span::styled(cursor_char.to_string(), cursor_style));
        if typed_count + 1 < end {
            spans.push(Span::styled(
                chars[typed_count + 1..end].iter().collect::<String>(),
                untyped_style,
            ));
        }
    } else if typed_len < line_len {
        spans.push(Span::styled(
            chars[start + typed_len..end].iter().collect::<String>(),
            untyped_style,
        ));
    }

    Line::from(spans)
}

fn wrap_text_ranges(chars: &[char], max_width: usize) -> Vec<(usize, usize)> {
    let mut ranges = Vec::new();
    if max_width == 0 {
        return ranges;
    }

    let mut idx = 0;
    while idx < chars.len() {
        if chars[idx] == '\n' {
            ranges.push((idx, idx));
            idx += 1;
            continue;
        }

        let mut end = (idx + max_width).min(chars.len());

        if let Some(newline_offset) = chars[idx..end].iter().position(|c| *c == '\n') {
            end = idx + newline_offset;
            ranges.push((idx, end));
            idx = end + 1;
            continue;
        }

        if end < chars.len() {
            if let Some(space_offset) = chars[idx..end].iter().rposition(|c| c.is_whitespace()) {
                let space_idx = idx + space_offset;
                if space_idx > idx {
                    ranges.push((idx, space_idx));
                    idx = space_idx + 1;
                    continue;
                }
            }
        }

        ranges.push((idx, end));
        idx = end;
        while idx < chars.len() && chars[idx].is_whitespace() && chars[idx] != '\n' {
            idx += 1;
        }
    }

    if ranges.is_empty() {
        ranges.push((0, 0));
    }

    ranges
}
