use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use directories::{ProjectDirs, UserDirs};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub const DEFAULT_DISCOVERY_PORT: u16 = 45_454;
pub const DEFAULT_TRANSFER_PORT: u16 = 45_455;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    pub device_id: Uuid,
    pub device_name: String,
    #[serde(default = "default_dark_mode")]
    pub dark_mode: bool,
    pub discovery_port: u16,
    pub transfer_port: u16,
    pub download_dir: PathBuf,
}

impl AppConfig {
    pub fn load() -> Self {
        Self::load_from_path(&config_path())
    }

    pub fn load_from_path(path: &Path) -> Self {
        match Self::load_from(path) {
            Ok(mut config) => {
                config.device_name = normalize_device_name(&config.device_name);
                if config.download_dir.as_os_str().is_empty() {
                    config.download_dir = default_download_dir();
                }
                config
            }
            Err(error) => {
                tracing::warn!(%error, "failed to load config; using defaults");
                Self::default()
            }
        }
    }

    pub fn save(&self) -> Result<()> {
        self.save_to_path(&config_path())
    }

    pub fn save_to_path(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).with_context(|| {
                format!("failed to create config directory: {}", parent.display())
            })?;
        }

        let json = serde_json::to_vec_pretty(self).context("failed to serialize configuration")?;
        fs::write(path, json)
            .with_context(|| format!("failed to write configuration: {}", path.display()))?;
        Ok(())
    }

    fn load_from(path: &Path) -> Result<Self> {
        let bytes = fs::read(path).with_context(|| format!("failed to read {}", path.display()))?;
        serde_json::from_slice(&bytes).context("configuration file is invalid")
    }
}

fn default_dark_mode() -> bool {
    true
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            device_id: Uuid::new_v4(),
            device_name: default_device_name(),
            dark_mode: default_dark_mode(),
            discovery_port: DEFAULT_DISCOVERY_PORT,
            transfer_port: DEFAULT_TRANSFER_PORT,
            download_dir: default_download_dir(),
        }
    }
}

pub fn config_path() -> PathBuf {
    ProjectDirs::from("io", "fasttran", "FastTran")
        .map(|dirs| dirs.config_dir().join("config.json"))
        .unwrap_or_else(|| PathBuf::from("fasttran-config.json"))
}

pub fn default_download_dir() -> PathBuf {
    UserDirs::new()
        .and_then(|dirs| dirs.download_dir().map(Path::to_path_buf))
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
        .join("FastTran")
}

pub fn normalize_device_name(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .filter(|character| !character.is_control())
        .collect::<String>()
        .trim()
        .chars()
        .take(32)
        .collect();

    if cleaned.is_empty() {
        default_device_name()
    } else {
        cleaned
    }
}

fn default_device_name() -> String {
    let raw = std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .unwrap_or_else(|_| "FastTran Device".to_owned());

    normalize_device_name(&raw)
}

#[cfg(test)]
mod tests {
    use super::{AppConfig, normalize_device_name};

    #[test]
    fn device_name_is_trimmed_and_bounded() {
        assert_eq!(normalize_device_name("  Alice's PC  "), "Alice's PC");
        assert_eq!(normalize_device_name(&"x".repeat(80)).chars().count(), 32);
        assert!(!normalize_device_name("\n\t").is_empty());
    }

    #[test]
    fn custom_config_path_round_trips() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("nested").join("config.json");
        let config = AppConfig {
            device_name: "Android Phone".to_owned(),
            ..AppConfig::default()
        };

        config.save_to_path(&path).unwrap();
        let loaded = AppConfig::load_from_path(&path);

        assert_eq!(loaded.device_name, "Android Phone");
        assert!(path.is_file());
    }
}
