use crate::db::{add_entity_to_database_as_transaction, get_entity_from_database};
use crate::structs::{Connection, GDPName};
use log::{error, info};
use std::collections::{HashMap, HashSet};
use std::str::FromStr;

const MAX_FANOUT: usize = 3;
const OVERLOADED_PENALTY: usize = 1000;

/// Get all publishers from Redis.
fn get_publishers(redis_url: &str, publishers_topic: &str) -> Result<Vec<GDPName>, String> {
    let publishers = get_entity_from_database(redis_url, publishers_topic)
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

/// Get all proxies from Redis.
fn get_proxies(redis_url: &str, proxy_topic: &str) -> Result<Vec<GDPName>, String> {
    let proxies = get_entity_from_database(redis_url, proxy_topic)
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
fn build_connections_map(
    redis_url: &str,
    connections_topic: &str,
) -> Result<HashMap<GDPName, HashSet<GDPName>>, String> {
    let connections = get_entity_from_database(redis_url, connections_topic)
        .map_err(|e| format!("Failed to get connections from Redis: {}", e))?;
    
    let mut connections_map = HashMap::new();
    for connection_str in connections {
        match Connection::from_str(&connection_str) {
            Ok(connection) => {
                connections_map
                    .entry(connection.publisher)
                    .or_insert_with(HashSet::new)
                    .insert(connection.subscriber);
            }
            Err(e) => {
                error!("Failed to parse connection '{}': {:?}", connection_str, e);
            }
        }
    }
    Ok(connections_map)
}

/// Get the current load (number of subscribers) for a given publisher/proxy.
fn get_current_load(
    connections_map: &HashMap<GDPName, HashSet<GDPName>>,
    candidate: &GDPName,
) -> usize {
    connections_map
        .get(candidate)
        .map(|subs| subs.len())
        .unwrap_or(0)
}

/// Find the least loaded candidate (publisher or proxy) that can accept a new subscriber.
fn find_least_loaded_candidate(
    candidates: &[GDPName],
    connections_map: &HashMap<GDPName, HashSet<GDPName>>,
) -> Option<GDPName> {
    candidates
        .iter()
        .min_by_key(|c| {
            let load = get_current_load(connections_map, c);
            if load >= MAX_FANOUT {
                load + OVERLOADED_PENALTY
            } else {
                load
            }
        })
        .copied()
}

/// Ensure a proxy has an upstream connection to a publisher.
fn ensure_proxy_upstream_to_publisher(
    redis_url: &str,
    connections_topic: &str,
    topic_name: &str,
    proxy: GDPName,
    publishers: &[GDPName],
    connections_map: &HashMap<GDPName, HashSet<GDPName>>,
) -> Result<(), String> {
    if publishers.is_empty() {
        return Ok(());
    }

    let upstream_pub = publishers
        .iter()
        .min_by_key(|p| get_current_load(connections_map, p));

    if let Some(upstream_pub) = upstream_pub {
        let already_linked = connections_map
            .get(upstream_pub)
            .map(|subs| subs.contains(&proxy))
            .unwrap_or(false);
        
        if !already_linked {
            let upstream_conn = format!("{}-{}", upstream_pub, proxy);
            info!(
                "Linking publisher {} to proxy {} on topic {}",
                upstream_pub, proxy, topic_name
            );
            add_entity_to_database_as_transaction(redis_url, connections_topic, &upstream_conn)
                .map_err(|e| format!("Failed to link publisher to proxy: {}", e))?;
        }
    }
    Ok(())
}

/// Ensure a proxy has an upstream connection to another proxy when no publishers exist.
fn ensure_proxy_upstream_to_proxy(
    redis_url: &str,
    connections_topic: &str,
    topic_name: &str,
    proxy: GDPName,
    proxies: &[GDPName],
    my_gdp_name: GDPName,
    connections_map: &HashMap<GDPName, HashSet<GDPName>>,
) -> Result<(), String> {
    let upstream_proxy = proxies
        .iter()
        .filter(|p| **p != proxy && **p != my_gdp_name)
        .min_by_key(|p| get_current_load(connections_map, p));

    if let Some(upstream_proxy) = upstream_proxy {
        let already_linked = connections_map
            .get(upstream_proxy)
            .map(|subs| subs.contains(&proxy))
            .unwrap_or(false);
        
        if !already_linked {
            let upstream_conn = format!("{}-{}", upstream_proxy, proxy);
            info!(
                "Linking proxy {} to upstream proxy {} on topic {}",
                proxy, upstream_proxy, topic_name
            );
            add_entity_to_database_as_transaction(redis_url, connections_topic, &upstream_conn)
                .map_err(|e| format!("Failed to link proxy to proxy: {}", e))?;
        }
    }
    Ok(())
}

// Attach a subscriber to a parent (publisher or proxy) while respecting a max fan-out of 3.
// Returns true if a connection was created.
pub fn attach_subscriber(
    redis_url: &str,
    connections_topic: &str,
    publishers_topic: &str,
    proxy_topic: &str,
    topic_name: &str,
    my_gdp_name: GDPName,
) -> bool {
    let publishers = match get_publishers(redis_url, publishers_topic) {
        Ok(p) => p,
        Err(e) => {
            error!("Error getting publishers: {}", e);
            return false;
        }
    };

    let connections_map = match build_connections_map(redis_url, connections_topic) {
        Ok(m) => m,
        Err(e) => {
            error!("Error building connections map: {}", e);
            return false;
        }
    };

    let proxies = match get_proxies(redis_url, proxy_topic) {
        Ok(p) => p,
        Err(e) => {
            error!("Error getting proxies: {}", e);
            return false;
        }
    };

    let mut candidates = publishers.clone();
    candidates.extend(proxies.clone());
    candidates.retain(|c| *c != my_gdp_name);

    if candidates.is_empty() {
        info!(
            "No publisher/proxy candidates available for {}; will retry later",
            my_gdp_name
        );
        return false;
    }

    let least_loaded = match find_least_loaded_candidate(&candidates, &connections_map) {
        Some(c) => c,
        None => {
            error!("No candidate found despite non-empty candidates list");
            return false;
        }
    };

    // If we attach to a proxy, ensure that proxy has an upstream connection.
    if proxies.contains(&least_loaded) {
        if !publishers.is_empty() {
            if let Err(e) = ensure_proxy_upstream_to_publisher(
                redis_url,
                connections_topic,
                topic_name,
                least_loaded,
                &publishers,
                &connections_map,
            ) {
                error!("Error ensuring proxy upstream to publisher: {}", e);
            }
        } else {
            // No publishers yet: link proxy to least-loaded other proxy if possible.
            if let Err(e) = ensure_proxy_upstream_to_proxy(
                redis_url,
                connections_topic,
                topic_name,
                least_loaded,
                &proxies,
                my_gdp_name,
                &connections_map,
            ) {
                error!("Error ensuring proxy upstream to proxy: {}", e);
            }
        }
    }

    let current_load = get_current_load(&connections_map, &least_loaded);

    if current_load >= MAX_FANOUT {
        info!(
            "Parent {} is at capacity ({}); subscriber {} will retry later",
            least_loaded, current_load, my_gdp_name
        );
        return false;
    }

    let connection = format!("{}-{}", least_loaded, my_gdp_name);
    info!(
        "Connecting subscriber {} to parent {} on topic {} (current load {})",
        my_gdp_name, least_loaded, topic_name, current_load
    );
    
    match add_entity_to_database_as_transaction(redis_url, connections_topic, &connection) {
        Ok(_) => true,
        Err(e) => {
            error!("Failed to add connection to Redis: {}", e);
            false
        }
    }
}
