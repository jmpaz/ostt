use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum VisualizationType {
    Waveform,
    #[default]
    Spectrum,
}

impl std::fmt::Display for VisualizationType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Waveform => write!(f, "waveform"),
            Self::Spectrum => write!(f, "spectrum"),
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct AudioConfig {
    pub device: String,
    pub sample_rate: u32,
    #[serde(default = "default_peak_volume_threshold")]
    pub peak_volume_threshold: u8,
    #[serde(default = "default_reference_level_db")]
    pub reference_level_db: i8,
    #[serde(default = "default_output_format")]
    pub output_format: String,
    #[serde(default)]
    pub visualization: VisualizationType,
}

fn default_output_format() -> String {
    "mp3 -ab 16k -ar 12000".to_string()
}

fn default_peak_volume_threshold() -> u8 {
    90
}

fn default_reference_level_db() -> i8 {
    -20
}

#[derive(Debug, Serialize, Deserialize)]
pub struct OsttConfig {
    pub audio: AudioConfig,
}

impl OsttConfig {
    pub fn load() -> Result<Self, Box<dyn std::error::Error>> {
        let config_path = get_config_path()?;
        let config_content = fs::read_to_string(&config_path)?;
        let config: Self = toml::from_str(&config_content)?;
        Ok(config)
    }

    pub fn save(&self) -> anyhow::Result<()> {
        let config_path = get_config_path()?;
        let config_content = toml::to_string_pretty(self)?;
        fs::write(&config_path, config_content)?;
        tracing::info!("Configuration saved");
        Ok(())
    }
}

fn get_config_path() -> Result<PathBuf, std::io::Error> {
    let config_dir = dirs::home_dir().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "Could not find home directory",
        )
    })?;
    let config_path = config_dir.join(".config").join("ostt").join("ostt.toml");

    if let Some(parent) = config_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    Ok(config_path)
}
