use crate::connection_store::{build_connections_map, get_proxies, get_publishers};
use crate::db::{add_entity_to_database_as_transaction, remove_entity_from_database_as_transaction};
use crate::structs::GDPName;
use log::{error, info, warn};
use std::collections::{HashMap, HashSet};

const MAX_CHILDREN: usize = 5;

/// Get number of children for a node.
fn child_count(connections: &HashMap<GDPName, HashSet<GDPName>>, node: &GDPName) -> usize {
    connections.get(node).map(|children| children.len()).unwrap_or(0)
}

/// Check if node has capacity for more children.
fn has_capacity(connections: &HashMap<GDPName, HashSet<GDPName>>, node: &GDPName) -> bool {
    child_count(connections, node) < MAX_CHILDREN
}

/// Find depth of node in tree (0 = publisher, 1+ = proxy/subscriber).
/// Returns None if node is not connected to any publisher.
fn node_depth(
    node: &GDPName,
    publishers: &[GDPName],
    connections: &HashMap<GDPName, HashSet<GDPName>>,
) -> Option<usize> {
    if publishers.contains(node) {
        return Some(0);
    }

    // BFS from publishers to find shortest path
    let mut queue = std::collections::VecDeque::new();
    let mut visited = HashSet::new();
    let mut depths = HashMap::new();

    for &pub_node in publishers {
        queue.push_back(pub_node);
        visited.insert(pub_node);
        depths.insert(pub_node, 0);
    }

    while let Some(current) = queue.pop_front() {
        if current == *node {
            return depths.get(&current).copied();
        }

        if let Some(children) = connections.get(&current) {
            let current_depth = *depths.get(&current).unwrap();
            for &child in children {
                if !visited.contains(&child) {
                    visited.insert(child);
                    depths.insert(child, current_depth + 1);
                    queue.push_back(child);
                }
            }
        }
    }

    None
}

/// Check if a node has mixed children (both proxies and subscribers).
fn has_mixed_children(
    node: &GDPName,
    proxies: &[GDPName],
    connections: &HashMap<GDPName, HashSet<GDPName>>,
) -> bool {
    if let Some(children) = connections.get(node) {
        let has_proxy = children.iter().any(|child| proxies.contains(child));
        let has_subscriber = children.iter().any(|child| !proxies.contains(child));
        has_proxy && has_subscriber
    } else {
        false
    }
}

/// Find the best upstream node for a new node (shallowest with capacity).
/// Prioritizes shallow nodes to keep tree depth minimal.
fn find_best_upstream(
    publishers: &[GDPName],
    proxies: &[GDPName],
    connections: &HashMap<GDPName, HashSet<GDPName>>,
    exclude: GDPName,
) -> Option<GDPName> {
    // Collect all nodes with capacity
    let mut candidates: Vec<GDPName> = publishers.iter()
        .chain(proxies.iter())
        .filter(|&&node| node != exclude && has_capacity(connections, &node))
        .copied()
        .collect();

    if candidates.is_empty() {
        return None;
    }

    // Sort by: 1) depth (shallowest first - strong priority), 2) load (least loaded first)
    candidates.sort_by(|a, b| {
        let depth_a = node_depth(a, publishers, connections).unwrap_or(usize::MAX);
        let depth_b = node_depth(b, publishers, connections).unwrap_or(usize::MAX);
        
        match depth_a.cmp(&depth_b) {
            std::cmp::Ordering::Equal => {
                child_count(connections, a).cmp(&child_count(connections, b))
            }
            other => other,
        }
    });

    candidates.first().copied()
}

/// Create a connection: parent -> child.
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

/// Connect a proxy to the tree by finding best upstream node.
/// Returns true if connection was created, false if already connected or no upstream available.
pub fn connect_proxy_to_tree(
    redis_url: &str,
    connections_key: &str,
    publishers_key: &str,
    proxies_key: &str,
    topic_name: &str,
    proxy_name: GDPName,
) -> Result<bool, String> {
    let publishers = get_publishers(redis_url, publishers_key)?;
    let proxies = get_proxies(redis_url, proxies_key)?;
    let connections = build_connections_map(redis_url, connections_key)?;

    // Check if already connected
    if node_depth(&proxy_name, &publishers, &connections).is_some() {
        return Ok(false);
    }

    // Find best upstream
    let upstream = match find_best_upstream(&publishers, &proxies, &connections, proxy_name) {
        Some(u) => u,
        None => {
            info!("No upstream available for proxy {} (topic: {}); will connect when available", proxy_name, topic_name);
            return Ok(false);
        }
    };

    create_connection(redis_url, connections_key, upstream, proxy_name)?;
    let node_type = if publishers.contains(&upstream) { "publisher" } else { "proxy" };
    info!("Connected proxy {} to {} {} (topic: {})", proxy_name, node_type, upstream, topic_name);
    
    Ok(true)
}

/// Ensure a node is connected to the tree (has path to publisher).
/// Returns true if node is connected, false otherwise.
fn ensure_node_connected(
    node: &GDPName,
    publishers: &[GDPName],
    connections: &HashMap<GDPName, HashSet<GDPName>>,
) -> bool {
    node_depth(node, publishers, connections).is_some()
}

/// Move all children from one parent to another.
fn move_children_to_new_parent(
    redis_url: &str,
    connections_key: &str,
    old_parent: GDPName,
    new_parent: GDPName,
    children: &HashSet<GDPName>,
) -> Result<(), String> {
    // Remove old connections
    for child in children {
        let old_conn = format!("{}-{}", old_parent, child);
        remove_entity_from_database_as_transaction(redis_url, connections_key, &old_conn)
            .map_err(|e| format!("Failed to remove old connection {}: {}", old_conn, e))?;
    }
    
    // Create new connections
    for child in children {
        create_connection(redis_url, connections_key, new_parent, *child)?;
    }
    
    Ok(())
}

/// Attach a subscriber to the tree.
/// Finds shallowest node with capacity and connects subscriber to it.
/// Ensures parent is connected to tree before attaching.
/// Prevents mixing proxies and subscribers as children of the same node.
pub fn attach_subscriber(
    redis_url: &str,
    connections_key: &str,
    publishers_key: &str,
    proxies_key: &str,
    topic_name: &str,
    subscriber_name: GDPName,
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

    let mut connections = match build_connections_map(redis_url, connections_key) {
        Ok(c) => c,
        Err(e) => {
            error!("Failed to build connections map: {}", e);
            return false;
        }
    };

    // Find best parent (shallowest node with capacity)
    let mut parent = match find_best_upstream(&publishers, &proxies, &connections, subscriber_name) {
        Some(p) => p,
        None => {
            info!("No parent with capacity for subscriber {} (topic: {}); will retry", subscriber_name, topic_name);
            return false;
        }
    };

    // If parent has mixed children (proxies and subscribers), find a proxy to take over
    if has_mixed_children(&parent, &proxies, &connections) {
        info!("Parent {} has mixed children, finding proxy to take over (topic: {})", parent, topic_name);
        
        // Find an available proxy that can take all children
        let children = connections.get(&parent).cloned().unwrap_or_default();
        let total_children = children.len();
        
        // Look for a proxy with enough capacity to take all children
        let candidate_proxy = proxies.iter()
            .find(|&&proxy| {
                proxy != parent && 
                proxy != subscriber_name &&
                has_capacity(&connections, &proxy) &&
                child_count(&connections, &proxy) + total_children <= MAX_CHILDREN
            })
            .copied();
        
        if let Some(proxy) = candidate_proxy {
            // Ensure proxy is connected
            if !ensure_node_connected(&proxy, &publishers, &connections) {
                if let Err(e) = connect_proxy_to_tree(
                    redis_url, connections_key, publishers_key, proxies_key, topic_name, proxy,
                ) {
                    error!("Failed to connect proxy {} to tree: {}", proxy, e);
                    return false;
                }
                
                // Reload connections
                connections = match build_connections_map(redis_url, connections_key) {
                    Ok(c) => c,
                    Err(e) => {
                        error!("Failed to reload connections: {}", e);
                        return false;
                    }
                };
            }
            
            // Move all children to the proxy
            if let Err(e) = move_children_to_new_parent(
                redis_url, connections_key, parent, proxy, &children,
            ) {
                error!("Failed to move children from {} to {}: {}", parent, proxy, e);
                return false;
            }
            
            info!("Moved {} children from {} to proxy {} (topic: {})", 
                children.len(), parent, proxy, topic_name);
            
            // Reload connections after moving
            connections = match build_connections_map(redis_url, connections_key) {
                Ok(c) => c,
                Err(e) => {
                    error!("Failed to reload connections after move: {}", e);
                    return false;
                }
            };
            
            parent = proxy;
        } else {
            // No suitable proxy found - this is a problem, but we'll proceed with warning
            warn!("Parent {} has mixed children but no suitable proxy found to take over (topic: {})", 
                parent, topic_name);
        }
    }

    // If parent is a proxy, ensure it's connected to tree
    if proxies.contains(&parent) && !ensure_node_connected(&parent, &publishers, &connections) {
        // Try to connect proxy to tree
        if let Err(e) = connect_proxy_to_tree(
            redis_url, connections_key, publishers_key, proxies_key, topic_name, parent,
        ) {
            error!("Failed to connect proxy {} to tree: {}", parent, e);
            return false;
        }
        
        // Reload connections after connecting proxy
        connections = match build_connections_map(redis_url, connections_key) {
            Ok(c) => c,
            Err(e) => {
                error!("Failed to reload connections: {}", e);
                return false;
            }
        };
        
        // Verify parent is now connected
        if !ensure_node_connected(&parent, &publishers, &connections) {
            error!("Proxy {} still not connected to tree after connection attempt", parent);
            return false;
        }
    }

    // Verify parent has capacity
    if !has_capacity(&connections, &parent) {
        info!("Parent {} at capacity for subscriber {} (topic: {})", parent, subscriber_name, topic_name);
        return false;
    }

    // Create connection
    let depth = node_depth(&parent, &publishers, &connections).unwrap_or(0);
    let load = child_count(&connections, &parent);
    info!(
        "Attaching subscriber {} to {} (depth: {}, load: {}/{}) on topic {}",
        subscriber_name, parent, depth, load, MAX_CHILDREN, topic_name
    );

    match create_connection(redis_url, connections_key, parent, subscriber_name) {
        Ok(_) => true,
        Err(e) => {
            error!("Failed to create connection: {}", e);
            false
        }
    }
}
