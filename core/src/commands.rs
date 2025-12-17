//! CLI command implementations.

use crate::topic_manager::ros_topic_discovery;
use utils::app_config::AppConfig;
use utils::error::Result;

#[tokio::main]
async fn router_async_loop() {
    let config = AppConfig::fetch().expect("AppConfig::fetch()");
    info!("{:#?}", config);
    ros_topic_discovery().await;
}

/// Start the SGC router: discover topics, establish WebRTC connections, and forward messages.
pub fn router() -> Result<()> {
    info!("router");
    router_async_loop();
    Ok(())
}

/// Print the current configuration.
pub fn config() -> Result<()> {
    let config = AppConfig::fetch()?;
    info!("{:#?}", config);
    Ok(())
}
