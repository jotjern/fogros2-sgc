use crate::connection_store::{build_connections_map, get_proxies, get_publishers};
use crate::db::add_entity_to_database_as_transaction;
use crate::structs::GDPName;
use log::{error, info};
use std::collections::{HashMap, HashSet};

const MAX_FANOUT: usize = 3;
const OVERLOADED_PENALTY: usize = 1000;

/// Represents the current state of the topic's routing topology.
pub struct TopicTopology {
    publishers: Vec<GDPName>,
    proxies: Vec<GDPName>,
    connections: HashMap<GDPName, HashSet<GDPName>>,
}

impl TopicTopology {
    /// Load topology from Redis.
    pub fn from_redis(
        redis_url: &str,
        publishers_key: &str,
        proxies_key: &str,
        connections_key: &str,
    ) -> Result<Self, String> {
        Ok(TopicTopology {
            publishers: get_publishers(redis_url, publishers_key)?,
            proxies: get_proxies(redis_url, proxies_key)?,
            connections: build_connections_map(redis_url, connections_key)?,
        })
    }

    /// Get current load (number of subscribers) for a node.
    fn load(&self, node: &GDPName) -> usize {
        self.connections.get(node).map(|subs| subs.len()).unwrap_or(0)
    }

    /// Check if a connection already exists.
    fn has_connection(&self, publisher: &GDPName, subscriber: &GDPName) -> bool {
        self.connections
            .get(publisher)
            .map(|subs| subs.contains(subscriber))
            .unwrap_or(false)
    }

    /// Get all candidate parents (publishers + proxies), excluding self.
    fn candidate_parents(&self, exclude: GDPName) -> Vec<GDPName> {
        let mut candidates: Vec<GDPName> = self.publishers.iter()
            .chain(self.proxies.iter())
            .filter(|&&c| c != exclude)
            .copied()
            .collect();
        candidates
    }

    /// Find the least loaded candidate that can accept a new subscriber.
    fn find_best_parent(&self, candidates: &[GDPName]) -> Option<GDPName> {
        candidates.iter().min_by_key(|c| {
            let load = self.load(c);
            if load >= MAX_FANOUT {
                load + OVERLOADED_PENALTY
            } else {
                load
            }
        }).copied()
    }
}

/// Ensure a proxy has an upstream connection.
fn ensure_proxy_upstream(
    redis_url: &str,
    connections_key: &str,
    topic_name: &str,
    proxy: GDPName,
    topology: &TopicTopology,
    my_gdp_name: GDPName,
) -> Result<(), String> {
    // Prefer connecting to a publisher if available
    if !topology.publishers.is_empty() {
        let upstream = topology.publishers.iter()
            .min_by_key(|p| topology.load(p));
        
        if let Some(&upstream) = upstream {
            if !topology.has_connection(&upstream, &proxy) {
                create_connection(redis_url, connections_key, upstream, proxy)?;
                info!("Linked publisher {} to proxy {} on topic {}", upstream, proxy, topic_name);
            }
        }
    } else if topology.proxies.len() > 1 {
        // No publishers: connect to another proxy
        let upstream = topology.proxies.iter()
            .filter(|&&p| p != proxy && p != my_gdp_name)
            .min_by_key(|p| topology.load(p));
        
        if let Some(&upstream) = upstream {
            if !topology.has_connection(&upstream, &proxy) {
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
    // Load current topology
    let topology = match TopicTopology::from_redis(redis_url, publishers_key, proxies_key, connections_key) {
        Ok(t) => t,
        Err(e) => {
            error!("Failed to load topic topology: {}", e);
            return false;
        }
    };

    // Find candidate parents
    let candidates = topology.candidate_parents(my_gdp_name);
    if candidates.is_empty() {
        info!("No available parents for subscriber {}; will retry later", my_gdp_name);
        return false;
    }

    // Select best parent
    let parent = match topology.find_best_parent(&candidates) {
        Some(p) => p,
        None => {
            error!("No parent found despite non-empty candidates");
            return false;
        }
    };

    // If parent is a proxy, ensure it has upstream connection
    if topology.proxies.contains(&parent) {
        if let Err(e) = ensure_proxy_upstream(redis_url, connections_key, topic_name, parent, &topology, my_gdp_name) {
            error!("Failed to ensure proxy upstream: {}", e);
        }
    }

    // Check if parent is at capacity
    let load = topology.load(&parent);
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
