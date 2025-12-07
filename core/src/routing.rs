use crate::connection_store::{build_connections_map, get_proxies, get_publishers};
use crate::db::add_entity_to_database_as_transaction;
use crate::structs::GDPName;
use log::{error, info};
use std::collections::{HashMap, HashSet};

const MAX_FANOUT: usize = 3;
const OVERLOADED_PENALTY: usize = 1000;

/// Get current load (number of subscribers) for a node.
fn get_load(connections: &HashMap<GDPName, HashSet<GDPName>>, node: &GDPName) -> usize {
    connections.get(node).map(|subs| subs.len()).unwrap_or(0)
}

/// Check if a connection already exists.
fn has_connection(connections: &HashMap<GDPName, HashSet<GDPName>>, publisher: &GDPName, subscriber: &GDPName) -> bool {
    connections
        .get(publisher)
        .map(|subs| subs.contains(subscriber))
        .unwrap_or(false)
}

/// Find the least loaded candidate that can accept a new subscriber.
fn find_best_parent(
    candidates: &[GDPName],
    connections: &HashMap<GDPName, HashSet<GDPName>>,
) -> Option<GDPName> {
    candidates.iter().min_by_key(|c| {
        let load = get_load(connections, c);
        if load >= MAX_FANOUT {
            load + OVERLOADED_PENALTY
        } else {
            load
        }
    }).copied()
}

/// Ensure a proxy has an upstream connection.
fn ensure_proxy_upstream(
    redis_url: &str,
    connections_key: &str,
    topic_name: &str,
    proxy: GDPName,
    publishers: &[GDPName],
    proxies: &[GDPName],
    connections: &HashMap<GDPName, HashSet<GDPName>>,
    my_gdp_name: GDPName,
) -> Result<(), String> {
    // Prefer connecting to a publisher if available
    if !publishers.is_empty() {
        let upstream = publishers.iter().min_by_key(|p| get_load(connections, p));
        if let Some(&upstream) = upstream {
            if !has_connection(connections, &upstream, &proxy) {
                create_connection(redis_url, connections_key, upstream, proxy)?;
                info!("Linked publisher {} to proxy {} on topic {}", upstream, proxy, topic_name);
            }
        }
    } else if proxies.len() > 1 {
        // No publishers: connect to another proxy
        let upstream = proxies.iter()
            .filter(|&&p| p != proxy && p != my_gdp_name)
            .min_by_key(|p| get_load(connections, p));
        if let Some(&upstream) = upstream {
            if !has_connection(connections, &upstream, &proxy) {
                create_connection(redis_url, connections_key, upstream, proxy)?;
                info!("Linked proxy {} to upstream proxy {} on topic {}", proxy, upstream, topic_name);
            }
        }
    }
    Ok(())
}

/// Create a connection in Redis.
fn create_connection(
    redis_url: &str,
    connections_key: &str,
    publisher: GDPName,
    subscriber: GDPName,
) -> Result<(), String> {
    let connection = format!("{}-{}", publisher, subscriber);
    add_entity_to_database_as_transaction(redis_url, connections_key, &connection)
        .map_err(|e| format!("Failed to create connection: {}", e))?;
    Ok(())
}

/// Attach a subscriber to the best available parent (publisher or proxy).
/// Returns true if a connection was successfully created.
pub fn attach_subscriber(
    redis_url: &str,
    connections_key: &str,
    publishers_key: &str,
    proxies_key: &str,
    topic_name: &str,
    my_gdp_name: GDPName,
) -> bool {
    // Load current state
    let publishers = match get_publishers(redis_url, publishers_key) {
        Ok(p) => p,
        Err(e) => {
            error!("Failed to get publishers: {}", e);
            return false;
        }
    };

    let proxies = match get_proxies(redis_url, proxies_key) {
        Ok(p) => p,
        Err(e) => {
            error!("Failed to get proxies: {}", e);
            return false;
        }
    };

    let connections = match build_connections_map(redis_url, connections_key) {
        Ok(c) => c,
        Err(e) => {
            error!("Failed to build connections map: {}", e);
            return false;
        }
    };

    // Find candidate parents (publishers + proxies, excluding self)
    let candidates: Vec<GDPName> = publishers.iter()
        .chain(proxies.iter())
        .filter(|&&c| c != my_gdp_name)
        .copied()
        .collect();

    if candidates.is_empty() {
        info!("No available parents for subscriber {}; will retry later", my_gdp_name);
        return false;
    }

    // Select best parent (least loaded)
    let parent = match find_best_parent(&candidates, &connections) {
        Some(p) => p,
        None => {
            error!("No parent found despite non-empty candidates");
            return false;
        }
    };

    // If parent is a proxy, ensure it has upstream connection
    if proxies.contains(&parent) {
        if let Err(e) = ensure_proxy_upstream(
            redis_url, connections_key, topic_name, parent,
            &publishers, &proxies, &connections, my_gdp_name,
        ) {
            error!("Failed to ensure proxy upstream: {}", e);
        }
    }

    // Check if parent is at capacity
    let load = get_load(&connections, &parent);
    if load >= MAX_FANOUT {
        info!("Parent {} is at capacity ({}); subscriber {} will retry later", parent, load, my_gdp_name);
        return false;
    }

    // Create connection
    info!("Connecting subscriber {} to parent {} on topic {} (load: {})", my_gdp_name, parent, topic_name, load);
    match create_connection(redis_url, connections_key, parent, my_gdp_name) {
        Ok(_) => true,
        Err(e) => {
            error!("Failed to create connection: {}", e);
            false
        }
    }
}
