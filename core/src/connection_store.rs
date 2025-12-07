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

/// Get all publishers from Redis for a topic.
pub fn get_publishers(redis_url: &str, publishers_key: &str) -> Result<Vec<GDPName>, String> {
    use crate::db::get_entity_from_database;
    let publishers = get_entity_from_database(redis_url, publishers_key)
        .map_err(|e| format!("Failed to get publishers from Redis: {}", e))?;
    
    let mut result = Vec::new();
    for gdp_name_string in publishers {
        match GDPName::from_str(&gdp_name_string) {
            Ok(name) => result.push(name),
            Err(e) => {
                error!("Failed to parse publisher GDP name '{}': {:?}", gdp_name_string, e);
            }
        }
    }
    Ok(result)
}

/// Get all proxies from Redis for a topic.
pub fn get_proxies(redis_url: &str, proxy_key: &str) -> Result<Vec<GDPName>, String> {
    use crate::db::get_entity_from_database;
    let proxies = get_entity_from_database(redis_url, proxy_key)
        .map_err(|e| format!("Failed to get proxies from Redis: {}", e))?;
    
    let mut result = Vec::new();
    for gdp_name_string in proxies {
        match GDPName::from_str(&gdp_name_string) {
            Ok(name) => result.push(name),
            Err(e) => {
                error!("Failed to parse proxy GDP name '{}': {:?}", gdp_name_string, e);
            }
        }
    }
    Ok(result)
}

/// Build a map of publisher -> set of subscribers from Redis connections.
pub fn build_connections_map(
    redis_url: &str,
    connections_key: &str,
) -> Result<std::collections::HashMap<GDPName, std::collections::HashSet<GDPName>>, String> {
    use crate::db::get_entity_from_database;
    let connections = get_entity_from_database(redis_url, connections_key)
        .map_err(|e| format!("Failed to get connections from Redis: {}", e))?;
    
    let mut connections_map = std::collections::HashMap::new();
    for connection_str in connections {
        match parse_connection(&connection_str) {
            Some(connection) => {
                connections_map
                    .entry(connection.publisher)
                    .or_insert_with(std::collections::HashSet::new)
                    .insert(connection.subscriber);
            }
            None => {
                // Error already logged in parse_connection
            }
        }
    }
    Ok(connections_map)
}
