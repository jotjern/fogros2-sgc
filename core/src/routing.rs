use crate::connection_store::{build_connections_map, get_proxies, get_publishers};
use crate::db::add_entity_to_database_as_transaction;
use crate::structs::GDPName;
use log::{error, info};
use std::collections::{HashMap, HashSet};

const MAX_CHILDREN: usize = 3;

/// Get current number of children for a node.
fn get_child_count(connections: &HashMap<GDPName, HashSet<GDPName>>, node: &GDPName) -> usize {
    connections.get(node).map(|children| children.len()).unwrap_or(0)
}

/// Check if a node has capacity (less than MAX_CHILDREN).
fn has_capacity(connections: &HashMap<GDPName, HashSet<GDPName>>, node: &GDPName) -> bool {
    get_child_count(connections, node) < MAX_CHILDREN
}

/// Calculate depth of a node in the tree (distance from nearest publisher).
fn calculate_depth(
    node: &GDPName,
    publishers: &[GDPName],
    connections: &HashMap<GDPName, HashSet<GDPName>>,
) -> Option<usize> {
    if publishers.contains(node) {
        return Some(0);
    }

    // BFS to find shortest path to any publisher
    let mut queue = std::collections::VecDeque::new();
    let mut visited = HashSet::new();
    let mut depth_map = HashMap::new();

    // Initialize with publishers at depth 0
    for &pub_node in publishers {
        queue.push_back(pub_node);
        visited.insert(pub_node);
        depth_map.insert(pub_node, 0);
    }

    while let Some(current) = queue.pop_front() {
        let current_depth = *depth_map.get(&current).unwrap();

        // Check if this is our target node
        if current == *node {
            return Some(current_depth);
        }

        // Add children to queue
        if let Some(children) = connections.get(&current) {
            for &child in children {
                if !visited.contains(&child) {
                    visited.insert(child);
                    depth_map.insert(child, current_depth + 1);
                    queue.push_back(child);
                }
            }
        }
    }

    None
}

/// Find the best parent node for a new subscriber, preferring shallower nodes with capacity.
fn find_best_parent(
    publishers: &[GDPName],
    proxies: &[GDPName],
    connections: &HashMap<GDPName, HashSet<GDPName>>,
    exclude: GDPName,
) -> Option<GDPName> {
    // Collect all candidate nodes (publishers + proxies)
    let mut candidates: Vec<GDPName> = publishers.iter()
        .chain(proxies.iter())
        .filter(|&&c| c != exclude && has_capacity(connections, &c))
        .copied()
        .collect();

    if candidates.is_empty() {
        return None;
    }

    // Sort by depth (shallowest first), then by child count (least loaded first)
    candidates.sort_by(|a, b| {
        let depth_a = calculate_depth(a, publishers, connections).unwrap_or(usize::MAX);
        let depth_b = calculate_depth(b, publishers, connections).unwrap_or(usize::MAX);
        
        match depth_a.cmp(&depth_b) {
            std::cmp::Ordering::Equal => {
                let load_a = get_child_count(connections, a);
                let load_b = get_child_count(connections, b);
                load_a.cmp(&load_b)
            }
            other => other,
        }
    });

    candidates.first().copied()
}

/// Ensure a proxy has an upstream connection to maintain tree structure.
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
    // Check if proxy already has an upstream connection
    let has_upstream = publishers.iter().any(|&p| {
        connections.get(&p).map(|children| children.contains(&proxy)).unwrap_or(false)
    }) || proxies.iter().any(|&p| {
        p != proxy && connections.get(&p).map(|children| children.contains(&proxy)).unwrap_or(false)
    });

    if has_upstream {
        return Ok(());
    }

    // Find best upstream node (prefer publisher, then shallowest proxy with capacity)
    let upstream = if !publishers.is_empty() {
        // Find publisher with capacity
        publishers.iter()
            .filter(|&&p| has_capacity(connections, &p))
            .min_by_key(|p| get_child_count(connections, p))
            .copied()
    } else {
        // Find proxy with capacity, excluding self and my_gdp_name
        proxies.iter()
            .filter(|&&p| p != proxy && p != my_gdp_name && has_capacity(connections, &p))
            .min_by_key(|p| {
                let depth = calculate_depth(p, publishers, connections).unwrap_or(usize::MAX);
                (depth, get_child_count(connections, p))
            })
            .copied()
    };

    if let Some(upstream) = upstream {
        create_connection(redis_url, connections_key, upstream, proxy)?;
        info!("Created upstream connection: {} -> {} (topic: {})", upstream, proxy, topic_name);
    } else {
        info!("No upstream node available for proxy {} (topic: {})", proxy, topic_name);
    }

    Ok(())
}

/// Create a connection in Redis.
fn create_connection(
    redis_url: &str,
    connections_key: &str,
    parent: GDPName,
    child: GDPName,
) -> Result<(), String> {
    let connection = format!("{}-{}", parent, child);
    add_entity_to_database_as_transaction(redis_url, connections_key, &connection)
        .map_err(|e| format!("Failed to create connection: {}", e))?;
    Ok(())
}

/// Connect a proxy to the tree by finding and connecting to an upstream node (publisher or proxy).
/// This ensures the proxy is part of the tree and can receive messages.
pub fn connect_proxy_to_tree(
    redis_url: &str,
    connections_key: &str,
    publishers_key: &str,
    proxies_key: &str,
    topic_name: &str,
    proxy_gdp_name: GDPName,
) -> Result<(), String> {
    // Load current state
    let publishers = get_publishers(redis_url, publishers_key)?;
    let proxies = get_proxies(redis_url, proxies_key)?;
    let connections = build_connections_map(redis_url, connections_key)?;

    // Check if proxy already has an upstream connection
    let has_upstream = publishers.iter().any(|&p| {
        connections.get(&p).map(|children| children.contains(&proxy_gdp_name)).unwrap_or(false)
    }) || proxies.iter().any(|&p| {
        p != proxy_gdp_name && connections.get(&p).map(|children| children.contains(&proxy_gdp_name)).unwrap_or(false)
    });

    if has_upstream {
        info!("Proxy {} already has upstream connection", proxy_gdp_name);
        return Ok(());
    }

    // Find best upstream node (prefer publisher, then shallowest proxy with capacity)
    let upstream = if !publishers.is_empty() {
        // Prefer connecting to a publisher with capacity
        publishers.iter()
            .filter(|&&p| has_capacity(&connections, &p))
            .min_by_key(|p| get_child_count(&connections, p))
            .copied()
    } else if !proxies.is_empty() {
        // No publishers: connect to shallowest proxy with capacity
        proxies.iter()
            .filter(|&&p| p != proxy_gdp_name && has_capacity(&connections, &p))
            .min_by_key(|p| {
                let depth = calculate_depth(p, &publishers, &connections).unwrap_or(usize::MAX);
                (depth, get_child_count(&connections, p))
            })
            .copied()
    } else {
        None
    };

    match upstream {
        Some(upstream_node) => {
            create_connection(redis_url, connections_key, upstream_node, proxy_gdp_name)?;
            let node_type = if publishers.contains(&upstream_node) { "publisher" } else { "proxy" };
            info!(
                "Connected proxy {} to {} {} (topic: {})",
                proxy_gdp_name, node_type, upstream_node, topic_name
            );
            Ok(())
        }
        None => {
            info!("No upstream node available for proxy {} (topic: {}); will connect when available", proxy_gdp_name, topic_name);
            Ok(())
        }
    }
}

/// Attach a subscriber to the tree structure, maintaining hierarchy with max 3 children per node.
/// Returns true if a connection was successfully created.
pub fn attach_subscriber(
    redis_url: &str,
    connections_key: &str,
    publishers_key: &str,
    proxies_key: &str,
    topic_name: &str,
    my_gdp_name: GDPName,
) -> bool {
    // Load current tree state
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

    let mut connections = match build_connections_map(redis_url, connections_key) {
        Ok(c) => c,
        Err(e) => {
            error!("Failed to build connections map: {}", e);
            return false;
        }
    };

    // Find best parent (shallowest node with capacity)
    let parent = match find_best_parent(&publishers, &proxies, &connections, my_gdp_name) {
        Some(p) => p,
        None => {
            info!("No available parent with capacity for subscriber {}; will retry later", my_gdp_name);
            return false;
        }
    };

    // If parent is a proxy, ensure it has upstream connection to maintain tree
    if proxies.contains(&parent) {
        if let Err(e) = ensure_proxy_upstream(
            redis_url, connections_key, topic_name, parent,
            &publishers, &proxies, &connections, my_gdp_name,
        ) {
            error!("Failed to ensure proxy upstream: {}", e);
            return false;
        }
        
        // Reload connections after ensuring upstream (may have added connection)
        connections = match build_connections_map(redis_url, connections_key) {
            Ok(c) => c,
            Err(e) => {
                error!("Failed to reload connections map: {}", e);
                return false;
            }
        };
    }

    // Verify parent has capacity
    if !has_capacity(&connections, &parent) {
        info!("Parent {} is at capacity; subscriber {} will retry later", parent, my_gdp_name);
        return false;
    }

    // Create connection in tree
    let depth = calculate_depth(&parent, &publishers, &connections).unwrap_or(0);
    let child_count = get_child_count(&connections, &parent);
    info!(
        "Attaching subscriber {} to parent {} (depth: {}, children: {}/{}) on topic {}",
        my_gdp_name, parent, depth, child_count, MAX_CHILDREN, topic_name
    );

    match create_connection(redis_url, connections_key, parent, my_gdp_name) {
        Ok(_) => true,
        Err(e) => {
            error!("Failed to create connection: {}", e);
            false
        }
    }
}
