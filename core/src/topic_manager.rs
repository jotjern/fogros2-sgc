use crate::network::ros::{network_to_ros_forwarder, ros_to_network_forwarder};
use crate::network::webrtc::{register_webrtc_stream, webrtc_reader_and_writer};
use crate::structs::{generate_random_gdp_name, get_gdp_name_from_topic, Connection, GDPName};

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::str::FromStr;
use tokio::sync::{
    broadcast,
    mpsc::unbounded_channel,
};
use tokio::time::Duration;
use utils::app_config::AppConfig;

use crate::db::*;


async fn handle_new_connection(
    connection: Connection, topic_name: &str, topic_type: &str, certificate: &Vec<u8>,
    my_gdp_name: GDPName, topic_gdp: GDPName,
) -> Option<(String, broadcast::Sender<()>)> {
    info!(
        "New connection in {} (gdp {}): {}",
        topic_name,
        topic_gdp,
        connection.to_string()
    );

    if connection.publisher != my_gdp_name && connection.subscriber != my_gdp_name {
        info!(
            "Im not involved: publisher {}, subscriber {}, me {}",
            connection.publisher, connection.subscriber, my_gdp_name
        );
        return None;
    }

    let publisher_signal_id = format!(
        "{}-{}-{}",
        topic_gdp, connection.publisher, connection.subscriber
    );
    let subscriber_signal_id = format!(
        "{}-{}-{}",
        topic_gdp, connection.subscriber, connection.publisher
    );
    let my_signal_id = if connection.publisher == my_gdp_name {
        publisher_signal_id.clone()
    } else {
        subscriber_signal_id.clone()
    };

    let signal_id_to_dial = if connection.publisher == my_gdp_name {
        None
    } else {
        Some(publisher_signal_id)
    };

    let (webrtc_stream, webrtc_shutdown) =
        register_webrtc_stream(&my_signal_id, signal_id_to_dial).await;

    println!("WebRTC connected!");

    let (ros_tx, ros_rx) = unbounded_channel();
    let (rtc_tx, rtc_rx) = unbounded_channel();

    let node_name = format!("ros_manager_node_{}", rand::random::<u32>());

    tokio::spawn(webrtc_reader_and_writer(webrtc_stream, ros_tx, rtc_rx));
    if connection.publisher == my_gdp_name {
        info!("Spawning ros to network forwarder");
        tokio::spawn(ros_to_network_forwarder(
            node_name,
            topic_name.to_owned(),
            topic_type.to_owned(),
            certificate.clone(),
            rtc_tx,
        ));
    } else {
        info!("Spawning network to ros forwarder");
        tokio::spawn(network_to_ros_forwarder(
            node_name,
            topic_name.to_owned(),
            topic_type.to_owned(),
            certificate.clone(),
            ros_rx,
        ));
    }

    Some((connection.to_string(), webrtc_shutdown))
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
            RedisListChange::Added(connection) => {
                let parsed = match Connection::from_str(connection.as_str()) {
                    Ok(c) => c,
                    Err(e) => {
                        error!("Failed to parse connection {}: {:?}", connection, e);
                        continue;
                    }
                };

                let topic_name_cloned = topic_name.clone();
                let topic_type_cloned = topic_type.clone();
                let certificate_cloned = certificate.clone();

                match tokio::spawn(async move {
                    handle_new_connection(
                        parsed,
                        &topic_name_cloned,
                        &topic_type_cloned,
                        &certificate_cloned,
                        my_gdp_name,
                        topic_gdp,
                    )
                    .await
                })
                .await
                {
                    Ok(Some((id, shutdown_sender))) => {
                        connections.insert(id, shutdown_sender);
                    }
                    Ok(None) => {}
                    Err(e) => error!("Failed to spawn connection handler: {:?}", e),
                }
            }
            RedisListChange::Removed(connection) => {
                if let Some(shutdown) = connections.remove(&connection) {
                    let _ = shutdown.send(());
                }
                info!(
                    "Removed remote subscriber from {}: {}",
                    connections_topic, connection
                );
            }
        }
    }
}

#[derive(Debug, PartialEq, Serialize, Deserialize, Clone)]
pub struct RosTopicStatus {
    pub action: String,
}

pub async fn ros_topic_discovery() {
    let config = AppConfig::fetch().unwrap();

    let certificate = std::fs::read(format!(
        "./scripts/crypto/{}/{}-private.pem",
        config.crypto_name, config.crypto_name
    ))
    .expect("crypto file missing");

    let ctx = r2r::Context::create().unwrap();
    let node = r2r::Node::create(ctx, "ros_manager", "namespace").unwrap();
    let my_gdp_name = generate_random_gdp_name();

    info!("My GDP name is: {}", my_gdp_name.to_string());

    for topic in config.ros {
        tokio::spawn(create_topic_network_bridge(
            topic.topic_name.clone(),
            topic.topic_type.clone(),
            certificate.clone(),
            my_gdp_name,
        ));

        let redis_url = get_redis_url();
        let topic_gdp = GDPName(get_gdp_name_from_topic(&topic.topic_name, &topic.topic_type, &certificate));
        let publishers_topic = format!("{}-publishers", topic_gdp);

        match topic.action.as_str() {
            "pub" => {
                add_entity_to_database_as_transaction(&redis_url, &publishers_topic, &my_gdp_name.to_string()).unwrap();
            },
            "sub" => {
                println!("TODO!");
            },
            "proxy" => {},
            "noop" => {}
            action => panic!("Unknown for topic {} action: {}", topic.topic_name, action)
        };
    }

    loop {
        tokio::time::sleep(Duration::from_millis(1000)).await;
    }
}
