use crate::db::{watch_redis_list_items, RedisListChange};
use crate::structs::{Connection, GDPName};
use log::error;
use log::info;
use std::str::FromStr;
use tokio::sync::mpsc::UnboundedReceiver;

/// Parse connection string, returning None on error.
pub fn parse_connection(connection_string: &str) -> Option<Connection> {
    Connection::from_str(connection_string).map_err(|e| {
        error!("Failed to parse connection {}: {:?}", connection_string, e);
        e
    }).ok()
}

/// Generate connection identifier for tracking connections.
pub fn connection_id(topic_gdp: GDPName, connection: &Connection) -> String {
    format!("{}-{}", topic_gdp, connection.to_string())
}

/// Generate Redis topic names for a given topic GDP.
pub fn topic_redis_keys(topic_gdp: GDPName) -> (String, String, String) {
    (
        format!("{}-publishers", topic_gdp),
        format!("{}-connections", topic_gdp),
        format!("{}-proxies", topic_gdp),
    )
}

/// Watch Redis for connection changes for a topic.
/// Returns a receiver that emits connection addition/removal events.
pub async fn watch_topic_connections(topic_gdp: GDPName) -> UnboundedReceiver<RedisListChange> {
    let connections_key = format!("{}-connections", topic_gdp);
    info!("Watching connections for topic GDP: {} (key: {})", topic_gdp, connections_key);
    watch_redis_list_items(connections_key).await
}

/// Check if this node is involved in the connection (either as publisher or subscriber).
pub fn is_node_involved(connection: &Connection, my_gdp_name: GDPName) -> bool {
    connection.publisher == my_gdp_name || connection.subscriber == my_gdp_name
}
