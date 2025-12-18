//! FogROS2-SGC: Secure Global Connectivity for ROS2
//!
//! Connects disjoint ROS2 networks across different locations using WebRTC.

#[cfg(not(debug_assertions))]
use human_panic::setup_panic;

#[cfg(debug_assertions)]
extern crate better_panic;

use std::env;
use std::fs;
use utils::app_config::AppConfig;
use utils::error::Result;

fn main() -> Result<()> {
    // Setup panic handlers
    #[cfg(not(debug_assertions))]
    {
        setup_panic!();
    }

    #[cfg(debug_assertions)]
    {
        better_panic::Settings::debug()
            .most_recent_first(false)
            .lineno_suffix(true)
            .verbosity(better_panic::Verbosity::Full)
            .install();
    }

    // Initialize logging (use RUST_LOG env var)
    if env::var("RUST_LOG").is_err() {
        env::set_var("RUST_LOG", "info");
    }
    env_logger::init();

    // Load config file
    let config_path = match env::var_os("SGC_CONFIG") {
        Some(config_file) => {
            format!("./src/resources/{}", config_file.into_string().unwrap())
        }
        None => "./src/resources/automatic.toml".to_owned(),
    };

    let config_contents = fs::read_to_string(&config_path).unwrap_or_else(|e| {
        eprintln!("Error: Cannot read config file '{}': {}", config_path, e);
        std::process::exit(1);
    });

    AppConfig::init(Some(&config_contents))?;

    cli::cli_match()
}
