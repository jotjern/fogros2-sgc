use crate::network::ros::{network_to_ros_forwarder, ros_to_network_forwarder};
use crate::network::webrtc::{register_webrtc_stream, webrtc_reader_and_writer};
use crate::routing::attach_subscriber;
use crate::structs::{Connection, GDPName, generate_random_gdp_name, get_gdp_name_from_topic};

use std::collections::HashMap;
use std::str::FromStr;
use rand::Rng;
use tokio::sync::{broadcast, mpsc::unbounded_channel};
use tokio::time::Duration;
use utils::app_config::AppConfig;

use crate::db::*;

// Constants for timing and delays
const SUBSCRIBER_ATTACH_BASE_DELAY_MS: u64 = 2000;
const SUBSCRIBER_ATTACH_RANDOM_DELAY_MS: u64 = 1000;
const MAIN_LOOP_SLEEP_MS: u64 = 1000;

/// Generate signal IDs for WebRTC signaling based on connection and topic GDP name.
/// Returns: (publisher_signal_id, subscriber_signal_id, my_signal_id, signal_id_to_dial)
fn generate_signal_ids(
    topic_gdp: GDPName,
    connection: &Connection,
    my_gdp_name: GDPName,
) -> (String, String, String, Option<String>) {
    let publisher_signal_id = format!(
        "{}-{}-{}",
        topic_gdp, connection.publisher, connection.subscriber
    );
    let subscriber_signal_id = format!(
        "{}-{}-{}",
        topic_gdp, connection.subscriber, connection.publisher
    );
    
    let is_publisher = connection.publisher == my_gdp_name;
    let my_signal_id = if is_publisher {
        publisher_signal_id.clone()
    } else {
        subscriber_signal_id.clone()
    };
    let signal_id_to_dial = if is_publisher {
        None
    } else {
        Some(publisher_signal_id.clone())
    };
    (publisher_signal_id, subscriber_signal_id, my_signal_id, signal_id_to_dial)
}

/// Set up WebRTC stream and ROS forwarding tasks for a connection.
async fn setup_webrtc_ros_bridge(
    my_signal_id: String,
    signal_id_to_dial: Option<String>,
    topic_name: String,
    topic_type: String,
    certificate: Vec<u8>,
    my_gdp_name: GDPName,
    is_publisher: bool,
    shutdown_tx: broadcast::Sender<()>,
) {
    let (webrtc_stream, webrtc_shutdown) =
        register_webrtc_stream(&my_signal_id, signal_id_to_dial).await;

    info!("WebRTC stream established for signal_id: {}", my_signal_id);

    let (ros_tx, ros_rx) = unbounded_channel();
    let (rtc_tx, rtc_rx) = unbounded_channel();

    let node_name = format!("ros_manager_node_{}", rand::random::<u32>());

    // Forward external shutdown to internal signaling tasks.
    let mut external_shutdown = shutdown_tx.subscribe();
    let webrtc_shutdown_clone = webrtc_shutdown.clone();
    tokio::spawn(async move {
        let _ = external_shutdown.recv().await;
        let _ = webrtc_shutdown_clone.send(());
    });

    tokio::spawn(webrtc_reader_and_writer(webrtc_stream, ros_tx, rtc_rx));
    if is_publisher {
        info!("Spawning ros to network forwarder");
        tokio::spawn(ros_to_network_forwarder(
            node_name,
            topic_name,
            topic_type,
            certificate,
            rtc_tx,
        ));
    } else {
        info!("Spawning network to ros forwarder");
        tokio::spawn(network_to_ros_forwarder(
            node_name,
            topic_name,
            topic_type,
            certificate,
            my_gdp_name,
            ros_rx,
        ));
    }
}

async fn handle_new_connection(
    connection: Connection,
    topic_name: &str,
    topic_type: &str,
    certificate: &[u8],
    my_gdp_name: GDPName,
    topic_gdp: GDPName,
) -> Option<broadcast::Sender<()>> {
    info!(
        "New connection in {} (gdp {}): {}",
        topic_name,
        topic_gdp,
        connection.to_string()
    );

    let (_publisher_signal_id, _subscriber_signal_id, my_signal_id, signal_id_to_dial) =
        generate_signal_ids(topic_gdp, &connection, my_gdp_name);

    // Expose shutdown immediately; set up WebRTC and ROS bridging in the background.
    let (shutdown_tx, _shutdown_rx) = broadcast::channel::<()>(1);
    let shutdown_tx_for_task = shutdown_tx.clone();
    let is_publisher = connection.publisher == my_gdp_name;

    tokio::spawn(async move {
        setup_webrtc_ros_bridge(
            my_signal_id,
            signal_id_to_dial,
            topic_name.to_owned(),
            topic_type.to_owned(),
            certificate.to_vec(),
            my_gdp_name,
            is_publisher,
            shutdown_tx_for_task,
        )
        .await;
    });

    Some(shutdown_tx)
}

/// Check if this node is involved in the connection (either as publisher or subscriber).
fn is_node_involved(connection: &Connection, my_gdp_name: GDPName) -> bool {
    connection.publisher == my_gdp_name || connection.subscriber == my_gdp_name
}

/// Generate connection identifier for tracking connections.
fn connection_identifier(topic_gdp: GDPName, connection: &Connection) -> String {
    format!("{}-{}", topic_gdp, connection.to_string())
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
    let connection = match Connection::from_str(&connection_string) {
        Ok(c) => c,
        Err(e) => {
            error!("Failed to parse connection {}: {:?}", connection_string, e);
            return;
        }
    };

    let connection_id = connection_identifier(topic_gdp, &connection);

    if !is_node_involved(&connection, my_gdp_name) {
        info!(
            "Skipping connection not involving this node: publisher {}, subscriber {}, me {}",
            connection.publisher, connection.subscriber, my_gdp_name
        );
        return;
    }

    match handle_new_connection(
        connection,
        topic_name,
        topic_type,
        certificate,
        my_gdp_name,
        topic_gdp,
    )
    .await
    {
        Some(shutdown_sender) => {
            connections.insert(connection_id.clone(), shutdown_sender);
            info!("Successfully established connection: {}", connection_id);
        }
        None => {
            error!("Failed to establish connection: {}", connection_id);
        }
    }
}

/// Handle a connection being removed from Redis.
fn handle_connection_removed(
    connection_string: String,
    topic_gdp: GDPName,
    connections_topic: &str,
    connections: &mut HashMap<String, broadcast::Sender<()>>,
) {
    let connection = match Connection::from_str(&connection_string) {
        Ok(c) => c,
        Err(e) => {
            error!("Failed to parse removed connection {}: {:?}", connection_string, e);
            return;
        }
    };

    let connection_id = connection_identifier(topic_gdp, &connection);

    if let Some(shutdown) = connections.remove(&connection_id) {
        let _ = shutdown.send(());
        info!("Shutdown signal sent for connection: {}", connection_id);
    }
    info!(
        "Connection removed from {}: {}",
        connections_topic, connection_string
    );
}

// This node receives from remote and publishes to local ROS
// 1. Check Redis for existing subscribers in {topic}-sub
// 2. For each subscriber, create WebRTC connection and listen
// 3. Watch Redis for new subscribers and connect dynamically
async fn create_topic_network_bridge(
    topic_name: String, topic_type: String, certificate: Vec<u8>, my_gdp_name: GDPName,
) {
    let topic_gdp = GDPName(get_gdp_name_from_topic(
        &topic_name,
        &topic_type,
        &certificate,
    ));
    let connections_topic = format!("{}-connections", topic_gdp);

    info!(
        "Topic {} has connection topic: {}",
        topic_name, connections_topic
    );

    let mut connection_changes = watch_redis_list_items(connections_topic.clone()).await;
    // Track both the bridge task and a shutdown signal for its WebRTC signaling tasks
    let mut connections: HashMap<String, broadcast::Sender<()>> = HashMap::new();

    while let Some(event) = connection_changes.recv().await {
        match event {
            RedisListChange::Added(connection_string) => {
                info!("New connection detected: {}", connection_string);
                handle_connection_added(
                    connection_string,
                    &topic_name,
                    &topic_type,
                    &certificate,
                    topic_gdp,
                    my_gdp_name,
                    &mut connections,
                )
                .await;
            }
            RedisListChange::Removed(connection_string) => {
                handle_connection_removed(
                    connection_string,
                    topic_gdp,
                    &connections_topic,
                    &mut connections,
                );
            }
        }
    }
}

/// Calculate topic GDP name from topic metadata.
fn calculate_topic_gdp(topic_name: &str, topic_type: &str, certificate: &[u8]) -> GDPName {
    GDPName(get_gdp_name_from_topic(topic_name, topic_type, certificate))
}

/// Generate Redis topic names for a given topic GDP.
fn generate_redis_topic_names(topic_gdp: GDPName) -> (String, String, String) {
    (
        format!("{}-publishers", topic_gdp),
        format!("{}-connections", topic_gdp),
        format!("{}-proxies", topic_gdp),
    )
}

/// Register this node as a publisher in Redis.
fn register_as_publisher(
    redis_url: &str,
    publishers_topic: &str,
    my_gdp_name: GDPName,
    topic_name: &str,
    topic_gdp: GDPName,
) -> Result<(), Box<dyn std::error::Error>> {
    add_entity_to_database_as_transaction(
        redis_url,
        publishers_topic,
        &my_gdp_name.to_string(),
    )
    .map_err(|e| format!("Failed to register as publisher: {}", e))?;
    info!("Registered as publisher for topic: {} (GDP: {})", topic_name, topic_gdp);
    Ok(())
}

/// Register this node as a proxy in Redis.
fn register_as_proxy(
    redis_url: &str,
    proxy_topic: &str,
    my_gdp_name: GDPName,
) -> Result<(), Box<dyn std::error::Error>> {
    add_entity_to_database_as_transaction(redis_url, proxy_topic, &my_gdp_name.to_string())
        .map_err(|e| format!("Failed to register as proxy: {}", e))?;
    info!("Registered as proxy (GDP: {})", my_gdp_name);
    Ok(())
}

/// Attach this node as a subscriber with a random delay to avoid thundering herd.
async fn attach_as_subscriber(
    redis_url: &str,
    connections_topic: &str,
    publishers_topic: &str,
    proxy_topic: &str,
    topic_name: &str,
    my_gdp_name: GDPName,
) {
    // Random delay to avoid thundering herd problem
    let delay_ms = SUBSCRIBER_ATTACH_BASE_DELAY_MS 
        + (rand::thread_rng().gen::<u64>() % SUBSCRIBER_ATTACH_RANDOM_DELAY_MS);
    tokio::time::sleep(Duration::from_millis(delay_ms)).await;
    
    if let Err(e) = attach_subscriber(
        redis_url,
        connections_topic,
        publishers_topic,
        proxy_topic,
        topic_name,
        my_gdp_name,
    ) {
        error!("Failed to attach as subscriber for topic {}: {:?}", topic_name, e);
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

    for topic in config.ros {
        let topic_gdp = calculate_topic_gdp(&topic.topic_name, &topic.topic_type, &certificate);
        
        // Spawn bridge task for this topic
        tokio::spawn(create_topic_network_bridge(
            topic.topic_name.clone(),
            topic.topic_type.clone(),
            certificate.clone(),
            my_gdp_name,
        ));

        let (publishers_topic, connections_topic, proxy_topic) =
            generate_redis_topic_names(topic_gdp);

        match topic.action.as_str() {
            "pub" => {
                if let Err(e) = register_as_publisher(
                    &redis_url,
                    &publishers_topic,
                    my_gdp_name,
                    &topic.topic_name,
                    topic_gdp,
                ) {
                    error!("Error registering as publisher: {}", e);
                }
            }
            "sub" => {
                tokio::spawn(attach_as_subscriber(
                    &redis_url,
                    &connections_topic,
                    &publishers_topic,
                    &proxy_topic,
                    &topic.topic_name,
                    my_gdp_name,
                ));
            }
            "proxy" => {
                if let Err(e) = register_as_proxy(&redis_url, &proxy_topic, my_gdp_name) {
                    error!("Error registering as proxy: {}", e);
                }
            }
            "noop" => {}
            action => {
                error!("Unknown action '{}' for topic {}", action, topic.topic_name);
            }
        }
    }

    loop {
        tokio::time::sleep(Duration::from_millis(MAIN_LOOP_SLEEP_MS)).await;
    }
}
