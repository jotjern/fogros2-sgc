//! Application configuration management.
//!
//! Loads configuration from TOML files. The config specifies:
//! - group_secret: Name of the shared secret directory (./secrets/{name}/)
//! - signaling_server: WebSocket URL for WebRTC signaling
//! - routing_server: Redis address for routing state
//! - topics: List of ROS topics to bridge

use config::{Config, Environment};
use lazy_static::{__Deref, lazy_static};
use serde::{Deserialize, Serialize};

use std::path::Path;
use std::sync::RwLock;

use super::error::Result;

lazy_static! {
    pub static ref CONFIG: RwLock<Config> = RwLock::new(Config::new());
}

/// Configuration for a single ROS topic to bridge.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Topic {
    /// ROS topic name (e.g., "/chatter")
    pub name: String,
    /// ROS message type (e.g., "std_msgs/msg/String")
    #[serde(rename = "type")]
    pub topic_type: String,
    /// Role: "publisher", "subscriber", or "proxy"
    pub role: String,
}

/// Main application configuration.
#[derive(Debug, Serialize, Deserialize)]
pub struct AppConfig {
    /// Name of the group secret directory under ./secrets/
    pub group_secret: String,
    /// WebSocket URL for the signaling server (e.g., "ws://signal.example.com:8000")
    pub signaling_server: String,
    /// Redis address for routing state (e.g., "rib.example.com:6379")
    pub routing_server: String,
    /// Topics to bridge between local ROS and remote peers
    pub topics: Vec<Topic>,
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

        // Allow environment variable overrides with SGC_ prefix
        settings.merge(Environment::with_prefix("SGC"))?;

        {
            let mut w = CONFIG.write()?;
            *w = settings;
        }

        Ok(())
    }

    /// Merge CLI args into config (currently unused).
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

    /// Get the path to the group secret file.
    pub fn secret_path(&self) -> std::path::PathBuf {
        std::path::PathBuf::from(format!("./secrets/{}/secret.key", self.group_secret))
    }

    /// Validate the configuration. Returns Ok(()) if valid, Err with details if not.
    pub fn validate(&self) -> std::result::Result<(), Vec<String>> {
        let mut errors = Vec::new();

        if self.group_secret.is_empty() {
            errors.push("group_secret cannot be empty".to_string());
        }

        if self.signaling_server.is_empty() {
            errors.push("signaling_server cannot be empty".to_string());
        } else if !self.signaling_server.starts_with("ws://")
            && !self.signaling_server.starts_with("wss://")
        {
            errors.push(format!(
                "signaling_server must start with ws:// or wss://, got: {}",
                self.signaling_server
            ));
        }

        if self.routing_server.is_empty() {
            errors.push("routing_server cannot be empty".to_string());
        }

        for (i, topic) in self.topics.iter().enumerate() {
            if topic.name.is_empty() {
                errors.push(format!("topics[{}].name cannot be empty", i));
            }
            if topic.topic_type.is_empty() {
                errors.push(format!("topics[{}].type cannot be empty", i));
            }
            if !["publisher", "subscriber", "proxy"].contains(&topic.role.as_str()) {
                errors.push(format!(
                    "topics[{}].role must be 'publisher', 'subscriber', or 'proxy', got: '{}'",
                    i, topic.role
                ));
            }
        }

        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }
}
