//! Application configuration management.

use config::{Config, Environment};
use lazy_static::{__Deref, lazy_static};
use serde::{Deserialize, Serialize};

use std::path::Path;
use std::sync::RwLock;

use super::error::Result;
use crate::types::LogLevel;

lazy_static! {
    pub static ref CONFIG: RwLock<Config> = RwLock::new(Config::new());
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Database {
    pub url: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ROS {
    pub action: String,
    pub topic_name: String,
    pub topic_type: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct AppConfig {
    pub debug: bool,
    pub log_level: LogLevel,
    pub crypto_name: String,
    pub signaling_server_address: String,
    pub routing_information_base_address: String,
    pub automatic_topic_discovery: bool,
    pub ros: Vec<ROS>,
}

impl AppConfig {
    /// Initialize AppConfig from TOML string.
    pub fn init(default_config: Option<&str>) -> Result<()> {
        let mut settings = Config::new();

        if let Some(config_contents) = default_config {
            settings.merge(config::File::from_str(
                config_contents,
                config::FileFormat::Toml,
            ))?;
        }

        settings.merge(Environment::with_prefix("APP"))?;

        {
            let mut w = CONFIG.write()?;
            *w = settings;
        }

        Ok(())
    }

    /// Merge CLI args into config (currently a no-op, config comes from file).
    pub fn merge_args(_app: clap::App) -> Result<()> {
        Ok(())
    }

    /// Merge additional config file.
    pub fn merge_config(config_file: Option<&Path>) -> Result<()> {
        if let Some(config_file_path) = config_file {
            CONFIG
                .write()?
                .merge(config::File::with_name(config_file_path.to_str().unwrap()))?;
        }
        Ok(())
    }

    pub fn set(key: &str, value: &str) -> Result<()> {
        CONFIG.write()?.set(key, value)?;
        Ok(())
    }

    pub fn get<'de, T>(key: &'de str) -> Result<T>
    where
        T: serde::Deserialize<'de>,
    {
        Ok(CONFIG.read()?.get::<T>(key)?)
    }

    pub fn fetch() -> Result<AppConfig> {
        let r = CONFIG.read()?;
        let config_clone = r.deref().clone();
        Ok(config_clone.try_into()?)
    }
}
