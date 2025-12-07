use crate::connection_store::{connection_id, is_node_involved, parse_connection, topic_redis_keys, watch_topic_connections};
use crate::db::{get_redis_url, register_proxy, register_publisher, RedisListChange};
use crate::network::ros::{network_to_ros_forwarder, ros_to_network_forwarder};
use crate::network::webrtc::{register_webrtc_stream, webrtc_reader_and_writer};
use crate::routing::attach_subscriber;
use crate::structs::{Connection, GDPName, generate_random_gdp_name, get_gdp_name_from_topic};

use std::collections::HashMap;
use rand::Rng;
use tokio::sync::{broadcast, mpsc::unbounded_channel};
use tokio::time::Duration;
use utils::app_config::AppConfig;

// Constants for timing and delays
const SUBSCRIBER_ATTACH_BASE_DELAY_MS: u64 = 2000;
const SUBSCRIBER_ATTACH_RANDOM_DELAY_MS: u64 = 1000;
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

    // Create shutdown channel for lifecycle control
    let (shutdown_tx, _shutdown_rx) = broadcast::channel::<()>(1);
    let shutdown_tx_for_webrtc = shutdown_tx.clone();

    // Spawn connection setup in background
    tokio::spawn(async move {
        let (webrtc_stream, webrtc_shutdown) =
            register_webrtc_stream(&my_signal_id, signal_id_to_dial).await;

        info!("WebRTC stream established for signal_id: {}", my_signal_id);

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

        tokio::spawn(webrtc_reader_and_writer(webrtc_stream, ros_tx, rtc_rx));
        
        if is_publisher {
            info!("Spawning ros to network forwarder");
            tokio::spawn(ros_to_network_forwarder(
                node_name, topic_name.to_owned(), topic_type.to_owned(),
                certificate.to_vec(), rtc_tx,
            ));
        } else {
            info!("Spawning network to ros forwarder");
            tokio::spawn(network_to_ros_forwarder(
                node_name, topic_name.to_owned(), topic_type.to_owned(),
                certificate.to_vec(), my_gdp_name, ros_rx,
            ));
        }
    });

    shutdown_tx
}

/// Handle a new connection being added to Redis.
async fn handle_connection_added(
    connection_string: String,
    topic_name: &str,
    topic_type: &str,
    certificate: &[u8],
    topic_gdp: GDPName,
    my_gdp_name: GDPName,
    connections: &mut HashMap<String, broadcast::Sender<()>>,
) {
    let connection = match parse_connection(&connection_string) {
        Some(c) => c,
        None => return,
    };

    // Skip if this node is not involved
    if !is_node_involved(&connection, my_gdp_name) {
        info!(
            "Skipping connection not involving this node: publisher {}, subscriber {}, me {}",
            connection.publisher, connection.subscriber, my_gdp_name
        );
        return;
    }

    let conn_id = connection_id(topic_gdp, &connection);
    let shutdown_tx = establish_connection(
        connection, topic_name, topic_type, certificate, my_gdp_name, topic_gdp,
    ).await;
    
    connections.insert(conn_id.clone(), shutdown_tx);
    info!("Successfully established connection: {}", conn_id);
}

/// Handle a connection being removed from Redis.
fn handle_connection_removed(
    connection_string: String,
    topic_gdp: GDPName,
    connections_topic: &str,
    connections: &mut HashMap<String, broadcast::Sender<()>>,
) {
    let connection = match parse_connection(&connection_string) {
        Some(c) => c,
        None => return,
    };

    let conn_id = connection_id(topic_gdp, &connection);
    if let Some(shutdown) = connections.remove(&conn_id) {
        let _ = shutdown.send(());
        info!("Shutdown signal sent for connection: {}", conn_id);
    }
    info!("Connection removed from {}: {}", connections_topic, connection_string);
}

/// Watch Redis for connection changes and manage connections for a topic.
/// This is the main event loop that reacts to connection additions/removals.
async fn manage_topic_connections(
    topic_name: String, topic_type: String, certificate: Vec<u8>, my_gdp_name: GDPName,
) {
    let topic_gdp = GDPName(get_gdp_name_from_topic(&topic_name, &topic_type, &certificate));
    let (_, connections_key, _) = topic_redis_keys(topic_gdp);

    info!("Managing connections for topic {} (GDP: {})", topic_name, topic_gdp);

    let mut connection_changes = watch_topic_connections(topic_gdp).await;
    let mut connections: HashMap<String, broadcast::Sender<()>> = HashMap::new();

    while let Some(event) = connection_changes.recv().await {
        match event {
            RedisListChange::Added(connection_string) => {
                info!("New connection detected: {}", connection_string);
                handle_connection_added(
                    connection_string, &topic_name, &topic_type, &certificate,
                    topic_gdp, my_gdp_name, &mut connections,
                ).await;
            }
            RedisListChange::Removed(connection_string) => {
                handle_connection_removed(
                    connection_string, topic_gdp, &connections_key, &mut connections,
                );
            }
        }
    }
}


/// Attach this node as a subscriber with a random delay to avoid thundering herd.
async fn attach_as_subscriber(
    redis_url: &str,
    topic_gdp: GDPName,
    topic_name: &str,
    my_gdp_name: GDPName,
) {
    // Random delay to avoid thundering herd problem
    let delay_ms = SUBSCRIBER_ATTACH_BASE_DELAY_MS 
        + (rand::thread_rng().gen::<u64>() % SUBSCRIBER_ATTACH_RANDOM_DELAY_MS);
    tokio::time::sleep(Duration::from_millis(delay_ms)).await;
    
    let (publishers_key, connections_key, proxy_key) = topic_redis_keys(topic_gdp);
    if let Err(e) = attach_subscriber(
        redis_url,
        &connections_key,
        &publishers_key,
        &proxy_key,
        topic_name,
        my_gdp_name,
    ) {
        error!("Failed to attach as subscriber for topic {}: {:?}", topic_name, e);
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
    
    // Always spawn connection manager to react to connection changes
    tokio::spawn(manage_topic_connections(
        topic_name.clone(), topic_type.clone(), certificate.clone(), my_gdp_name,
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
                &redis_url, topic_gdp, &topic_name, my_gdp_name,
            ));
        }
        "proxy" => {
            if let Err(e) = register_proxy(&redis_url, topic_gdp, my_gdp_name) {
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
