use crate::connection_store::{connection_id, is_node_involved, parse_connection};
use crate::db::{get_redis_url, watch_redis_key, RedisKeyChange};
use crate::network::ros::{network_to_ros_forwarder, ros_to_network_forwarder};
use crate::network::webrtc::{register_webrtc_stream, webrtc_reader_and_writer};
use crate::routing::{current_connections, register_proxy, register_publisher, subscribe};
use crate::structs::{Connection, GDPName, generate_random_gdp_name, get_gdp_name_from_topic};

use std::collections::HashMap;
use rand::Rng;
use tokio::sync::{broadcast, mpsc::unbounded_channel};
use tokio::time::Duration;
use utils::app_config::AppConfig;

// Constants for timing and delays
const SUBSCRIBER_ATTACH_BASE_DELAY_MS: u64 = 2000;
const SUBSCRIBER_ATTACH_RANDOM_DELAY_MS: u64 = 5000;
const MAIN_LOOP_SLEEP_MS: u64 = 1000;

/// Generate signal ID for this node and optionally the ID to dial (if subscriber).
fn generate_signal_ids(
    topic_gdp: GDPName,
    connection: &Connection,
    my_gdp_name: GDPName,
) -> (String, Option<String>) {
    let is_publisher = connection.publisher == my_gdp_name;
    let my_signal_id = if is_publisher {
        format!("{}-{}-{}", topic_gdp, connection.publisher, connection.subscriber)
    } else {
        format!("{}-{}-{}", topic_gdp, connection.subscriber, connection.publisher)
    };
    let signal_id_to_dial = if is_publisher {
        None
    } else {
        Some(format!("{}-{}-{}", topic_gdp, connection.publisher, connection.subscriber))
    };
    (my_signal_id, signal_id_to_dial)
}

/// Establish a WebRTC connection and set up ROS forwarding.
/// Returns a shutdown channel to control the connection lifecycle.
async fn establish_connection(
    connection: Connection,
    topic_name: &str,
    topic_type: &str,
    certificate: &[u8],
    my_gdp_name: GDPName,
    topic_gdp: GDPName,
) -> broadcast::Sender<()> {
    info!(
        "Establishing connection in {} (gdp {}): {}",
        topic_name, topic_gdp, connection.to_string()
    );

    let (my_signal_id, signal_id_to_dial) = generate_signal_ids(topic_gdp, &connection, my_gdp_name);
    let is_publisher = connection.publisher == my_gdp_name;

    // Clone data needed in spawned task
    let topic_name_owned = topic_name.to_owned();
    let topic_type_owned = topic_type.to_owned();
    let certificate_owned = certificate.to_vec();

    // Create shutdown channel for lifecycle control
    let (shutdown_tx, _shutdown_rx) = broadcast::channel::<()>(1);
    let shutdown_tx_for_webrtc = shutdown_tx.clone();

    // Spawn connection setup in background
    tokio::spawn(async move {
        info!("[Connection] Starting WebRTC setup for signal_id: {}", my_signal_id);
        let (webrtc_stream, webrtc_shutdown) = match register_webrtc_stream(&my_signal_id, signal_id_to_dial).await {
            (stream, shutdown) => {
                info!("[Connection] WebRTC stream established successfully for signal_id: {}", my_signal_id);
                (stream, shutdown)
            }
        };

        let (ros_tx, ros_rx) = unbounded_channel();
        let (rtc_tx, rtc_rx) = unbounded_channel();
        let node_name = format!("ros_manager_node_{}", rand::random::<u32>());

        // Forward shutdown signal to WebRTC
        let mut shutdown_rx = shutdown_tx_for_webrtc.subscribe();
        let webrtc_shutdown_clone = webrtc_shutdown.clone();
        tokio::spawn(async move {
            let _ = shutdown_rx.recv().await;
            let _ = webrtc_shutdown_clone.send(());
        });

        info!("[Connection] Spawning WebRTC reader/writer for signal_id: {}", my_signal_id);
        tokio::spawn(webrtc_reader_and_writer(webrtc_stream, ros_tx, rtc_rx));
        
        if is_publisher {
            info!("[Connection] Spawning ROS to network forwarder for signal_id: {}", my_signal_id);
            tokio::spawn(ros_to_network_forwarder(
                node_name, topic_name_owned, topic_type_owned,
                certificate_owned, rtc_tx,
            ));
        } else {
            info!("[Connection] Spawning network to ROS forwarder for signal_id: {}", my_signal_id);
            tokio::spawn(network_to_ros_forwarder(
                node_name, topic_name_owned, topic_type_owned,
                certificate_owned, my_gdp_name, ros_rx,
            ));
        }
        
        info!("[Connection] All tasks spawned successfully for signal_id: {}", my_signal_id);
    });

    shutdown_tx
}

/// Handle a new connection being added to Redis.
/// Returns true if this node is the publisher in this connection.
async fn handle_connection_added(
    connection_string: String,
    topic_name: &str,
    topic_type: &str,
    certificate: &[u8],
    topic_gdp: GDPName,
    my_gdp_name: GDPName,
    connections: &mut HashMap<String, broadcast::Sender<()>>,
    _redis_url: &str,
    is_publisher: bool,
    is_proxy: bool,
) -> bool {
    let connection = match parse_connection(&connection_string) {
        Some(c) => c,
        None => return false,
    };

    // Skip if this node is not involved
    if !is_node_involved(&connection, my_gdp_name) {
        info!(
            "Skipping connection not involving this node: publisher {}, subscriber {}, me {}",
            connection.publisher, connection.subscriber, my_gdp_name
        );
        return false;
    }

    let conn_id = connection_id(topic_gdp, &connection);
    let shutdown_tx = establish_connection(
        connection.clone(), topic_name, topic_type, certificate, my_gdp_name, topic_gdp,
    ).await;
    
    connections.insert(conn_id.clone(), shutdown_tx);
    info!("Successfully established connection: {}", conn_id);
    
    let is_this_node_publisher = connection.publisher == my_gdp_name;
    let _ = (is_publisher, is_proxy);
    
    is_this_node_publisher
}

/// Handle a connection being removed from Redis.
/// Returns true if this node was the publisher in this connection.
fn handle_connection_removed(
    connection_string: String,
    topic_gdp: GDPName,
    connections_topic: &str,
    connections: &mut HashMap<String, broadcast::Sender<()>>,
    _redis_url: &str,
    my_gdp_name: GDPName,
    is_publisher: bool,
    is_proxy: bool,
) -> bool {
    let connection = match parse_connection(&connection_string) {
        Some(c) => c,
        None => return false,
    };

    let conn_id = connection_id(topic_gdp, &connection);
    if let Some(shutdown) = connections.remove(&conn_id) {
        let _ = shutdown.send(());
        info!("Shutdown signal sent for connection: {}", conn_id);
    }
    info!("Connection removed from {}: {}", connections_topic, connection_string);
    
    let is_this_node_publisher = connection.publisher == my_gdp_name;
    let _ = (is_publisher, is_proxy);
    
    is_this_node_publisher
}

/// Watch Redis for connection changes and manage connections for a topic.
/// This is the main event loop that reacts to connection additions/removals.
async fn manage_topic_connections(
    topic_name: String, topic_type: String, certificate: Vec<u8>, my_gdp_name: GDPName,
    is_publisher: bool, is_proxy: bool,
) {
    let topic_gdp = GDPName(get_gdp_name_from_topic(&topic_name, &topic_type, &certificate));
    let redis_url = get_redis_url();
    let routing_key = format!("{}-routing", topic_gdp);

    info!("Managing connections for topic {} (GDP: {})", topic_name, topic_gdp);

    let mut routing_changes = watch_redis_key(routing_key.clone()).await;
    let mut connections: HashMap<String, broadcast::Sender<()>> = HashMap::new();
    let mut current_edge_set: std::collections::HashSet<String> = std::collections::HashSet::new();

    // Initial reconcile (in case the key already exists).
    let _ = routing_changes.try_recv();

    while let Some(RedisKeyChange { key: _, event: _ }) = routing_changes.recv().await {
        let all = match current_connections(&redis_url, topic_gdp) {
            Ok(v) => v,
            Err(e) => {
                error!("Failed to read routing state for topic {}: {}", topic_name, e);
                continue;
            }
        };

        // Keep a copy of full routing edges for rejoin decisions below.
        let all_edges = all;

        let relevant: Vec<Connection> = all_edges
            .iter()
            .cloned()
            .filter(|c| is_node_involved(c, my_gdp_name))
            .collect();

        let mut new_edge_set: std::collections::HashSet<String> = std::collections::HashSet::new();
        for c in &relevant {
            new_edge_set.insert(c.to_string());
        }

        // Removed edges
        for removed in current_edge_set.difference(&new_edge_set).cloned().collect::<Vec<_>>() {
            let _ = handle_connection_removed(
                removed.clone(),
                topic_gdp,
                &routing_key,
                &mut connections,
                &redis_url,
                my_gdp_name,
                is_publisher,
                is_proxy,
            );
        }

        // Added edges
        for added in new_edge_set.difference(&current_edge_set).cloned().collect::<Vec<_>>() {
            let _was_publisher = handle_connection_added(
                added,
                &topic_name,
                &topic_type,
                &certificate,
                topic_gdp,
                my_gdp_name,
                &mut connections,
                &redis_url,
                is_publisher,
                is_proxy,
            )
            .await;
        }

        current_edge_set = new_edge_set;

        // -------------------------------------------------------------
        // Re-join policy (prevents flapping during atomic moves):
        //
        // Only re-subscribe if, in the *current* routing state, we have
        // NO parent edge (i.e. no inbound connection to `my_gdp_name`).
        // Do NOT re-subscribe just because one particular edge was removed;
        // graft/move operations remove+add edges in the same atomic update.
        // -------------------------------------------------------------
        let has_parent_now = all_edges.iter().any(|c| c.subscriber == my_gdp_name);
        let has_children_now = all_edges.iter().any(|c| c.publisher == my_gdp_name);

        if !has_parent_now {
            // Listener: ensure exactly one inbound parent edge exists.
            if !is_publisher && !is_proxy {
                tokio::spawn(attach_as_subscriber(
                    redis_url.clone(),
                    topic_gdp,
                    topic_name.clone(),
                    my_gdp_name,
                ));
            }

            // Proxy: only attempt to rejoin if we currently have downstream children,
            // meaning we are acting as an intermediate and must have an upstream parent.
            if is_proxy && has_children_now {
                let redis_url_for_retry = redis_url.clone();
                let topic_name_for_retry = topic_name.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_millis(500)).await;
                    let _ = subscribe(&redis_url_for_retry, topic_gdp, &topic_name_for_retry, my_gdp_name);
                });
            }
        }
    }
}


/// Attach this node as a subscriber with a random delay to avoid thundering herd.
async fn attach_as_subscriber(
    redis_url: String,
    topic_gdp: GDPName,
    topic_name: String,
    my_gdp_name: GDPName,
) {
    // Random delay to avoid thundering herd problem
    let delay_ms = SUBSCRIBER_ATTACH_BASE_DELAY_MS 
        + (rand::thread_rng().gen::<u64>() % SUBSCRIBER_ATTACH_RANDOM_DELAY_MS);
    tokio::time::sleep(Duration::from_millis(delay_ms)).await;
    
    if let Err(e) = subscribe(&redis_url, topic_gdp, &topic_name, my_gdp_name) {
        error!("Failed to subscribe for topic {}: {}", topic_name, e);
    }
}

/// Setup a topic: spawn connection watcher and register role.
fn setup_topic(
    topic_name: String,
    topic_type: String,
    topic_action: String,
    certificate: Vec<u8>,
    my_gdp_name: GDPName,
    redis_url: String,
) {
    let topic_gdp = GDPName(get_gdp_name_from_topic(&topic_name, &topic_type, &certificate));
    
    // Determine if this node is a publisher or proxy
    let is_publisher = topic_action == "pub";
    let is_proxy = topic_action == "proxy";
    
    // Always spawn connection manager to react to connection changes
    tokio::spawn(manage_topic_connections(
        topic_name.clone(), topic_type.clone(), certificate.clone(), my_gdp_name,
        is_publisher, is_proxy,
    ));

    // Register role based on action
    match topic_action.as_str() {
        "pub" => {
            if let Err(e) = register_publisher(&redis_url, topic_gdp, my_gdp_name, &topic_name) {
                error!("Error registering as publisher: {}", e);
            }
        }
        "sub" => {
            tokio::spawn(attach_as_subscriber(
                redis_url.clone(), topic_gdp, topic_name.clone(), my_gdp_name,
            ));
        }
        "proxy" => {
            if let Err(e) = register_proxy(&redis_url, topic_gdp, my_gdp_name, &topic_name) {
                error!("Error registering as proxy: {}", e);
            }
        }
        "noop" => {}
        action => error!("Unknown action '{}' for topic {}", action, topic_name),
    }
}

pub async fn ros_topic_discovery() {
    let config = AppConfig::fetch().unwrap_or_else(|e| {
        error!("Failed to fetch app config: {}", e);
        std::process::exit(1);
    });

    let certificate = std::fs::read(format!(
        "./scripts/crypto/{}/{}-private.pem",
        config.crypto_name, config.crypto_name
    ))
    .unwrap_or_else(|e| {
        error!(
            "Failed to read certificate file for crypto_name '{}': {}",
            config.crypto_name, e
        );
        std::process::exit(1);
    });

    let ctx = r2r::Context::create().unwrap_or_else(|e| {
        error!("Failed to create ROS context: {}", e);
        std::process::exit(1);
    });
    let _node = r2r::Node::create(ctx, "ros_manager", "namespace").unwrap_or_else(|e| {
        error!("Failed to create ROS node: {}", e);
        std::process::exit(1);
    });
    let my_gdp_name = generate_random_gdp_name();

    info!("My GDP name is: {}", my_gdp_name.to_string());

    let redis_url = get_redis_url();
    
    // Publish GDP name -> Docker container name mapping
    let container_name = crate::db::get_container_name();
    if let Err(e) = crate::db::publish_gdp_name_mapping(&redis_url, my_gdp_name, &container_name) {
        error!("Failed to publish GDP name mapping: {}", e);
    } else {
        info!("Published GDP name mapping: {} -> {}", my_gdp_name.to_string(), container_name);
    }
    
    info!("Node {} ({}) registered but not yet connected to any topics", my_gdp_name.to_string(), container_name);

    // Setup each topic: spawn watcher and register role
    for topic in config.ros {
        setup_topic(
            topic.topic_name.clone(),
            topic.topic_type.clone(),
            topic.action.clone(),
            certificate.clone(),
            my_gdp_name,
            redis_url.clone(),
        );
    }

    loop {
        tokio::time::sleep(Duration::from_millis(MAIN_LOOP_SLEEP_MS)).await;
    }
}
