use crate::structs::{Connection, GDPName};
use log::error;
use std::str::FromStr;

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

// (Removed) list-based Redis connection watching and helpers.
// Routing state is now `{topic}-routing` JSON and is watched directly by `topic_manager.rs`.

/// Check if this node is involved in the connection (either as publisher or subscriber).
pub fn is_node_involved(connection: &Connection, my_gdp_name: GDPName) -> bool {
    connection.publisher == my_gdp_name || connection.subscriber == my_gdp_name
}

// (Removed) list-based publisher/proxy queries and connection map builders.
