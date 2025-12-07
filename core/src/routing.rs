use crate::connection_store::{
    build_connections_map, get_distressed_nodes, get_proxies, get_publishers,
};
use crate::db::{
    add_entity_to_database_as_transaction, remove_entity_from_database_as_transaction,
    register_publisher,
};
use crate::structs::GDPName;
use log::{error, info, warn};
use std::collections::{HashMap, HashSet};
use std::str::FromStr;

/*
Routing model (simplified)
--------------------------
- Distress thresholds: publishers are distressed with >=3 direct children, proxies with >=5.
- Leaf publisher: publisher with no publisher children.
- Subscriber placement:
    * Pick the non-distressed leaf with the fewest direct listeners (ties by GDPName).
    * Never attach to a distressed publisher; if all leaves are distressed, attach a proxy to
      the least-loaded distressed leaf (if available) and connect to that proxy. Otherwise retry.
- Distress handling (run every second by each node):
    * If distressed, move one listener to the least-loaded non-distressed proxy child.
    * Otherwise, attach one available non-distressed proxy, promote it, and move one listener.
    * Only one listener is moved per tick to limit churn.
*/

#[derive(Clone, Debug)]
struct LeafPublisherView {
    publisher: GDPName,
    listener_count: usize,
    distressed: bool,
}

// Distress thresholds
const PUBLISHER_DISTRESS_THRESHOLD: usize = 3;
const PROXY_DISTRESS_THRESHOLD: usize = 5;

/// Add a node to the distress list
fn add_to_distress(
    redis_url: &str,
    distress_key: &str,
    node: GDPName,
) -> Result<(), String> {
    add_entity_to_database_as_transaction(redis_url, distress_key, &node.to_string())
        .map_err(|e| format!("Failed to add node to distress list: {}", e))?;
    info!("Marked node {} as distressed", node);
    Ok(())
}

/// Remove a node from the distress list
fn remove_from_distress(
    redis_url: &str,
    distress_key: &str,
    node: GDPName,
) -> Result<(), String> {
    remove_entity_from_database_as_transaction(redis_url, distress_key, &node.to_string())
        .map_err(|e| format!("Failed to remove node from distress list: {}", e))?;
    info!("Removed node {} from distress list", node);
    Ok(())
}

/// Update distress status for this node based on local connection count.
/// This should be called by publishers/proxies when their connection count changes.
pub fn update_own_distress_status(
    redis_url: &str,
    topic_gdp: GDPName,
    my_gdp_name: GDPName,
    connection_count: usize,
    is_proxy: bool,
) -> Result<(), String> {
    let (_, _, _, distress_key) = crate::connection_store::topic_redis_keys(topic_gdp);
    
    // Determine threshold
    let threshold = if is_proxy {
        PROXY_DISTRESS_THRESHOLD
    } else {
        PUBLISHER_DISTRESS_THRESHOLD
    };
    
    // Check current distress status
    let distressed = get_distressed_nodes(redis_url, &distress_key)
        .unwrap_or_default();
    let is_currently_distressed = distressed.contains(&my_gdp_name);
    
    // Update distress status based on local connection count
    if connection_count >= threshold && !is_currently_distressed {
        // Mark as distressed
        add_to_distress(redis_url, &distress_key, my_gdp_name)?;
    } else if connection_count < threshold && is_currently_distressed {
        // Remove from distress
        remove_from_distress(redis_url, &distress_key, my_gdp_name)?;
    }
    
    Ok(())
}

/// Find leaf publishers (publishers with no publisher children) with listener counts and distress info.
/// Listener count only includes direct listener children (not proxies, not publishers).
fn collect_leaf_publishers(
    redis_url: &str,
    connections_key: &str,
    publishers_key: &str,
    proxies_key: &str,
    distress_key: &str,
) -> Result<Vec<LeafPublisherView>, String> {
    let connections_map = build_connections_map(redis_url, connections_key)?;
    let publishers = get_publishers(redis_url, publishers_key)?;
    let proxies = get_proxies(redis_url, proxies_key)?;
    let distressed = get_distressed_nodes(redis_url, distress_key)?;

    let publishers_set: HashSet<GDPName> = publishers.iter().copied().collect();
    let proxies_set: HashSet<GDPName> = proxies.iter().copied().collect();

    let mut leaf_publishers = Vec::new();

    for publisher in publishers {
        // Check if this publisher has any children that are also publishers
        let has_publisher_children = connections_map
            .get(&publisher)
            .map(|subscribers| subscribers.iter().any(|&sub| publishers_set.contains(&sub)))
            .unwrap_or(false);

        // A leaf publisher has no children that are publishers
        if !has_publisher_children {
            // Count only direct listener children (not proxies, not publishers)
            let listener_count = connections_map
                .get(&publisher)
                .map(|subscribers| {
                    subscribers
                        .iter()
                        .filter(|&&sub| !proxies_set.contains(&sub) && !publishers_set.contains(&sub))
                        .count()
                })
                .unwrap_or(0);

            leaf_publishers.push(LeafPublisherView {
                publisher,
                listener_count,
                distressed: distressed.contains(&publisher),
            });
        }
    }

    Ok(leaf_publishers)
}

fn choose_leaf_for_subscriber(leaves: &[LeafPublisherView]) -> Option<LeafPublisherView> {
    leaves
        .iter()
        .filter(|leaf| !leaf.distressed)
        .min_by(|a, b| {
            a.listener_count
                .cmp(&b.listener_count)
                .then_with(|| a.publisher.cmp(&b.publisher))
        })
        .cloned()
}

fn connection_load(
    connections_map: &HashMap<GDPName, HashSet<GDPName>>,
    node: GDPName,
) -> usize {
    connections_map
        .get(&node)
        .map(|subs| subs.len())
        .unwrap_or(0)
}

/// Connect a subscriber to the best available leaf publisher.
pub fn attach_subscriber_to_publisher(
    redis_url: &str,
    connections_key: &str,
    publishers_key: &str,
    proxies_key: &str,
    topic_name: &str,
    subscriber_name: GDPName,
) -> Result<bool, String> {
    let distress_key = format!("{}-distress", publishers_key.strip_suffix("-publishers").unwrap_or(publishers_key));

    // Get current connection state for up-to-date listener counts
    let connections_map = build_connections_map(redis_url, connections_key)?;
    let proxies = get_proxies(redis_url, proxies_key)?;
    let distressed = get_distressed_nodes(redis_url, &distress_key)?;

    // Find all leaf publishers with listener counts
    let leaf_publishers = collect_leaf_publishers(redis_url, connections_key, publishers_key, proxies_key, &distress_key)?;

    if leaf_publishers.is_empty() {
        info!("No leaf publishers available for subscriber {} (topic: {}); will retry", subscriber_name, topic_name);
        return Ok(false);
    }

    if let Some(selected_leaf) = choose_leaf_for_subscriber(&leaf_publishers) {
        let connection = format!("{}-{}", selected_leaf.publisher, subscriber_name);
        add_entity_to_database_as_transaction(redis_url, connections_key, &connection)
            .map_err(|e| format!("Failed to create connection: {}", e))?;

        info!(
            "Connected subscriber {} to publisher {} (topic: {}, listeners: {}, distressed: {})",
            subscriber_name,
            selected_leaf.publisher,
            topic_name,
            selected_leaf.listener_count,
            selected_leaf.distressed
        );
        return Ok(true);
    }

    // All leaves are distressed. Attach a proxy to the least-loaded distressed leaf if possible.
    let distressed_target = leaf_publishers
        .iter()
        .filter(|leaf| leaf.distressed)
        .min_by(|a, b| {
            a.listener_count
                .cmp(&b.listener_count)
                .then_with(|| a.publisher.cmp(&b.publisher))
        })
        .cloned();

    let topic_gdp = match GDPName::from_str(
        publishers_key
            .strip_suffix("-publishers")
            .unwrap_or(publishers_key),
    ) {
        Ok(gdp) => gdp,
        Err(e) => {
            warn!(
                "Cannot parse topic GDP from key {} ({}); refusing to connect subscriber {}",
                publishers_key, e, subscriber_name
            );
            return Ok(false);
        }
    };

    if let Some(target_leaf) = distressed_target {
        if let Some(proxy) = attach_proxy_to_distressed_publisher(
            redis_url,
            connections_key,
            topic_gdp,
            topic_name,
            &connections_map,
            &proxies,
            &distressed,
            target_leaf.publisher,
        )? {
            let connection = format!("{}-{}", proxy, subscriber_name);
            add_entity_to_database_as_transaction(redis_url, connections_key, &connection)
                .map_err(|e| format!("Failed to create connection to proxy: {}", e))?;

            info!(
                "Connected subscriber {} to proxy {} (topic: {}, parent was distressed publisher {})",
                subscriber_name, proxy, topic_name, target_leaf.publisher
            );
            return Ok(true);
        }
    }

    warn!(
        "No safe publisher or proxy path available for subscriber {} on topic {}",
        subscriber_name, topic_name
    );
    Ok(false)
}

/// When a proxy connects to a publisher, make the proxy a publisher too.
pub fn make_proxy_publisher(
    redis_url: &str,
    topic_gdp: GDPName,
    topic_name: &str,
    proxy_name: GDPName,
) -> Result<(), String> {
    let publishers_key = format!("{}-publishers", topic_gdp);
    let publishers = get_publishers(redis_url, &publishers_key)?;
    
    // Check if proxy is already a publisher
    if publishers.contains(&proxy_name) {
        return Ok(());
    }
    
    // Register proxy as publisher
    register_publisher(redis_url, topic_gdp, proxy_name, topic_name)
        .map_err(|e| format!("Failed to register proxy as publisher: {}", e))?;
    
    info!("Proxy {} became a publisher for topic {}", proxy_name, topic_name);
    Ok(())
}

/// Move one direct listener from a parent to another parent node.
fn move_listener(
    redis_url: &str,
    connections_key: &str,
    from_parent: GDPName,
    to_parent: GDPName,
    subscriber: GDPName,
) -> Result<(), String> {
    let old_connection = format!("{}-{}", from_parent, subscriber);
    remove_entity_from_database_as_transaction(redis_url, connections_key, &old_connection)
        .map_err(|e| format!("Failed to remove old connection: {}", e))?;

    let new_connection = format!("{}-{}", to_parent, subscriber);
    add_entity_to_database_as_transaction(redis_url, connections_key, &new_connection)
        .map_err(|e| format!("Failed to add new connection: {}", e))?;

    info!(
        "Moved subscriber {} from {} to {}",
        subscriber, from_parent, to_parent
    );
    Ok(())
}

/// Attach a proxy to a distressed publisher and promote it.
/// Returns Ok(Some(proxy)) if attached, Ok(None) otherwise.
fn attach_proxy_to_distressed_publisher(
    redis_url: &str,
    connections_key: &str,
    topic_gdp: GDPName,
    topic_name: &str,
    connections_map: &HashMap<GDPName, HashSet<GDPName>>,
    proxies: &[GDPName],
    distressed: &HashSet<GDPName>,
    distressed_parent: GDPName,
) -> Result<Option<GDPName>, String> {
    let proxies_set: HashSet<GDPName> = proxies.iter().copied().collect();

    // Do not attach a proxy under another proxy; keep tree depth minimal.
    if proxies_set.contains(&distressed_parent) {
        warn!(
            "Refusing to attach proxy under proxy {} to avoid deepening the tree",
            distressed_parent
        );
        return Ok(None);
    }

    let attached_proxies: HashSet<GDPName> = connections_map
        .values()
        .flat_map(|subs| subs.iter().copied())
        .filter(|node| proxies_set.contains(node))
        .collect();

    let mut available_proxies: Vec<GDPName> = proxies
        .iter()
        .copied()
        .filter(|p| !distressed.contains(p))
        .filter(|p| !attached_proxies.contains(p))
        .collect();

    if available_proxies.is_empty() {
        warn!(
            "No available non-distressed proxies to attach to parent {}",
            distressed_parent
        );
        return Ok(None);
    }

    // Choose the least loaded available proxy (ties by GDPName) to balance the tree
    available_proxies.sort_by(|a, b| {
        connection_load(connections_map, *a)
            .cmp(&connection_load(connections_map, *b))
            .then_with(|| a.cmp(b))
    });

    let proxy = available_proxies[0];
    let connection = format!("{}-{}", distressed_parent, proxy);
    add_entity_to_database_as_transaction(redis_url, connections_key, &connection)
        .map_err(|e| format!("Failed to attach proxy to parent {}: {}", distressed_parent, e))?;
    
    make_proxy_publisher(redis_url, topic_gdp, topic_name, proxy)?;
    
    info!("Attached proxy {} to {}", proxy, distressed_parent);
    Ok(Some(proxy))
}

/// Handle distress for a publisher: move subscribers to proxies.
pub fn handle_distressed_publisher(
    redis_url: &str,
    topic_gdp: GDPName,
    topic_name: &str,
    distressed_publisher: GDPName,
) -> Result<(), String> {
    let (publishers_key, connections_key, proxies_key, distress_key) = 
        crate::connection_store::topic_redis_keys(topic_gdp);
    
    let publishers = get_publishers(redis_url, &publishers_key)?;
    let proxies = get_proxies(redis_url, &proxies_key)?;
    let publishers_set: HashSet<GDPName> = publishers.iter().copied().collect();
    let proxies_set: HashSet<GDPName> = proxies.iter().copied().collect();
    
    if !publishers_set.contains(&distressed_publisher) && !proxies_set.contains(&distressed_publisher) {
        return Ok(());
    }
    
    let connections_map = build_connections_map(redis_url, &connections_key)?;
    let distressed = get_distressed_nodes(redis_url, &distress_key)?;

    if !distressed.contains(&distressed_publisher) {
        return Ok(());
    }

    let child_count = connections_map
        .get(&distressed_publisher)
        .map(|subs| subs.len())
        .unwrap_or(0);

    // Distressed proxies do not re-parent listeners; only publishers move listeners to their own children.
    if proxies_set.contains(&distressed_publisher) {
        return Ok(());
    }

    let listener_children: Vec<GDPName> = connections_map
        .get(&distressed_publisher)
        .map(|subscribers| {
            subscribers
                .iter()
                .filter(|&&sub| !proxies_set.contains(&sub) && !publishers_set.contains(&sub))
                .copied()
                .collect()
        })
        .unwrap_or_default();

    if listener_children.is_empty() {
        return Ok(());
    }

    let proxy_children: Vec<GDPName> = connections_map
        .get(&distressed_publisher)
        .map(|subscribers| {
            subscribers
                .iter()
                .filter(|&&sub| proxies_set.contains(&sub))
                .copied()
                .collect()
        })
        .unwrap_or_default();

    if let Some(target_proxy) = proxy_children
        .iter()
        .filter(|p| !distressed.contains(*p))
        .min_by(|a, b| {
            connection_load(&connections_map, **a)
                .cmp(&connection_load(&connections_map, **b))
                .then_with(|| a.cmp(b))
        })
        .copied()
    {
        let subscriber = listener_children
            .iter()
            .copied()
            .min()
            .ok_or_else(|| "Expected at least one listener to move".to_string())?;

        move_listener(
            redis_url,
            &connections_key,
            distressed_publisher,
            target_proxy,
            subscriber,
        )?;

        let remaining_children = child_count.saturating_sub(1);
        if remaining_children >= PUBLISHER_DISTRESS_THRESHOLD {
            let _ = attach_proxy_to_distressed_publisher(
                redis_url,
                &connections_key,
                topic_gdp,
                topic_name,
                &connections_map,
                &proxies,
                &distressed,
                distressed_publisher,
            )?;
        }

        return Ok(());
    }

    if let Some(proxy) = attach_proxy_to_distressed_publisher(
        redis_url,
        &connections_key,
        topic_gdp,
        topic_name,
        &connections_map,
        &proxies,
        &distressed,
        distressed_publisher,
    )? {
        let subscriber = listener_children
            .iter()
            .copied()
            .min()
            .ok_or_else(|| "Expected at least one listener to move".to_string())?;

        move_listener(
            redis_url,
            &connections_key,
            distressed_publisher,
            proxy,
            subscriber,
        )?;
    }
    
    Ok(())
}

/// Handle distress for this node (publisher or proxy).
/// This function should be spawned as a background task and runs every 1 second.
/// It checks if this node is distressed and handles it accordingly.
pub async fn handle_own_distress(
    redis_url: String,
    topic_gdp: GDPName,
    topic_name: String,
    my_gdp_name: GDPName,
) {
    use tokio::time::{interval, Duration};
    
    let mut interval = interval(Duration::from_secs(1));
    
    loop {
        interval.tick().await;
        
        let (_, _, _, distress_key) = crate::connection_store::topic_redis_keys(topic_gdp);
        
        // Check if this node is distressed
        let distressed = match get_distressed_nodes(&redis_url, &distress_key) {
            Ok(d) => d,
            Err(e) => {
                error!("Failed to get distressed nodes: {}", e);
                continue;
            }
        };
        
        // Only handle if this node is distressed
        if distressed.contains(&my_gdp_name) {
            if let Err(e) = handle_distressed_publisher(&redis_url, topic_gdp, &topic_name, my_gdp_name) {
                error!("Failed to handle own distress for {}: {}", my_gdp_name, e);
            }
        }
    }
}
