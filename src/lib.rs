//! ostt - Open Speech-to-Text
//!
//! An interactive terminal-based audio recording and speech-to-text transcription tool.
//!
//! ostt allows you to:
//! - Record audio with real-time waveform visualization and volume metering
//! - Transcribe recordings by shelling out to `contextualize`
//! - Maintain a searchable history of all transcriptions
//! - Keep recent recordings available during the cache window

pub mod app;
pub mod clipboard;
pub mod commands;
pub mod config;
pub mod history;
pub mod keywords;
pub mod logging;
pub mod recording;
pub mod remote;
pub mod setup;
pub mod ui;

pub use app::run;
