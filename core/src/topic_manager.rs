use crate::network::ros::{ros_publisher, ros_subscriber};
use crate::network::webrtc::{register_webrtc_stream, webrtc_reader_and_writer};
use crate::structs::{generate_random_gdp_name, get_gdp_name_from_topic, Connection, GDPName};

use async_datachannel::DataStream;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::str::FromStr;
use tokio::process::Command;
use tokio::sync::{
    broadcast,
    mpsc::{unbounded_channel, UnboundedReceiver},
};
use tokio::task::JoinHandle;
use tokio::time::{sleep, Duration};
use utils::app_config::{AppConfig, ROS};

use crate::db::*;
use futures::StreamExt;
use redis_async::client;

enum RedisListChange {
    Added(String),
    Removed(String),
}

async fn watch_redis_list_items(list_key: String) -> UnboundedReceiver<RedisListChange> {
    let redis_url = get_redis_url();
    allow_keyspace_notification(&redis_url).unwrap();

    let (host, port) = get_redis_address_and_port();
    let pubsub = client::pubsub_connect(host, port)
        .await
        .expect("Cannot connect to Redis pubsub");

    let keyspace_topic = format!("__keyspace@0__:{}", list_key);
    let mut stream = pubsub
        .psubscribe(&keyspace_topic)
        .await
        .expect("Cannot subscribe");

    let (tx, rx) = unbounded_channel();
    let mut known_items = HashSet::<String>::new();

    tokio::spawn(async move {
        while !tx.is_closed() {
            let items: HashSet<String> = get_entity_from_database(&redis_url, &list_key)
                .unwrap_or_default()
                .into_iter()
                .collect();

            for item in &items {
                if known_items.insert(item.clone()) {
                    let _ = tx.send(RedisListChange::Added(item.clone()));
                }
            }

            let to_remove: Vec<String> = known_items
                .iter()
                .filter(|item| !items.contains(*item))
                .cloned()
                .collect();

            for item in to_remove {
                known_items.remove(&item);
                let _ = tx.send(RedisListChange::Removed(item));
            }

            // Wait for a notification from the redis server
            loop {
                match stream.next().await {
                    Some(Ok(_)) => break,
                    Some(Err(e)) => error!("Error when waiting for redis updates: {}", e),
                    None => (),
                }
            }
        }
    });

    rx
}

async fn handle_new_connection(
    connection: Connection, topic_name: &str, topic_type: &str, certificate: &[u8],
    my_gdp_name: GDPName, topic_gdp: GDPName,
) -> Option<(String, (JoinHandle<()>, broadcast::Sender<()>))> {
    info!(
        "New connection in {} (gdp {}): {}",
        topic_name,
        topic_gdp,
        connection.to_string()
    );

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

    // Initiator should target the *other* peer's ID, not its own.
    let signal_id_to_dial = if connection.publisher == my_gdp_name {
        None
    } else {
        Some(publisher_signal_id)
    };

    info!("I am {}, dialing: {:?}", my_signal_id, signal_id_to_dial);

    let action = (if connection.publisher == my_gdp_name {
        "pub"
    } else {
        "sub"
    })
    .to_owned();

    let (webrtc_stream, webrtc_shutdown) =
        register_webrtc_stream(&my_signal_id, signal_id_to_dial).await;

    println!("WebRTC connected!");

    let topic_join_handle = ros_topic_creator(
        webrtc_stream,
        format!("ros_manager_node_{}", rand::random::<u32>()),
        topic_name.to_string(),
        topic_type.to_string(),
        action,
        certificate.to_vec(),
    )
    .await;
    info!("Amazing");

    Some((connection.to_string(), (topic_join_handle, webrtc_shutdown)))
}

// Determine if this node should publish to the remote or subscribe from the remote
// "pub" = no local subscribers, so we have local publishers and should send outbound
// "sub" = no local publishers, so we should receive from remote and publish locally
// "noop" = both exist locally, no bridging needed
/// Currently it uses cli to get the information
/// TODO: use r2r/rcl to get the information
async fn determine_topic_action(topic_name: String) -> String {
    let out = Command::new("ros2")
        .arg("topic")
        .arg("info")
        .arg(topic_name.as_str())
        .output()
        .await
        .unwrap();

    let output_str = String::from_utf8(out.stdout).unwrap();

    if output_str.contains("Publisher count: 0") {
        "sub".into()
    } else if output_str.contains("Subscription count: 0") {
        "pub".into()
    } else {
        "noop".into()
    }
}

// Bridge between WebRTC DataStream and local ROS topic.
// Returns a JoinHandle so callers can abort the whole bridge.
pub async fn ros_topic_creator(
    stream: DataStream, node_name: String, topic_name: String, topic_type: String, action: String,
    certificate: Vec<u8>,
) -> JoinHandle<()> {
    info!(
        "topic creator for topic {}, type {}, action {}",
        topic_name, topic_type, action
    );
    // ros_tx/rx: messages from WebRTC to ROS
    // rtc_tx/rx: messages from ROS to WebRTC
    let (ros_tx, ros_rx) = unbounded_channel();
    let (rtc_tx, rtc_rx) = unbounded_channel();
    tokio::spawn(async move {
        let webrtc_fut = webrtc_reader_and_writer(stream, ros_tx, rtc_rx);
        tokio::pin!(webrtc_fut);

        // Action semantics:
        // - "pub": we are the publisher -> read from local ROS and send over WebRTC.
        // - "sub": we are the subscriber -> read from WebRTC and publish to local ROS.
        match action.as_str() {
            "pub" => {
                let ros_fut =
                    ros_subscriber(node_name, topic_name, topic_type, certificate, rtc_tx);
                tokio::pin!(ros_fut);
                tokio::select! {
                    _ = &mut webrtc_fut => {},
                    _ = &mut ros_fut => {},
                }
            }
            "sub" => {
                let ros_fut = ros_publisher(node_name, topic_name, topic_type, certificate, ros_rx);
                tokio::pin!(ros_fut);
                tokio::select! {
                    _ = &mut webrtc_fut => {},
                    _ = &mut ros_fut => {},
                }
            }
            _ => panic!("unknown action"),
        };
    })
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
    let mut connections: HashMap<String, (JoinHandle<()>, broadcast::Sender<()>)> = HashMap::new();

    loop {
        match connection_changes.recv().await {
            None => break,
            Some(RedisListChange::Added(connection)) => {
                let parsed = match Connection::from_str(connection.as_str()) {
                    Ok(c) => c,
                    Err(e) => {
                        error!("Failed to parse connection {}: {:?}", connection, e);
                        continue;
                    }
                };

                if parsed.publisher != my_gdp_name && parsed.subscriber != my_gdp_name {
                    info!(
                        "Im not involved: publisher {}, subscriber {}, me {}",
                        parsed.publisher, parsed.subscriber, my_gdp_name
                    );
                    continue;
                }

                // Spawn connection setup but await it here; clone owned data for 'static future.
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
                    Ok(Some((id, handles))) => {
                        connections.insert(id, handles);
                    }
                    Ok(None) => {}
                    Err(e) => error!("Failed to spawn connection handler: {:?}", e),
                }
            }
            Some(RedisListChange::Removed(connection)) => {
                if let Some((handle, shutdown)) = connections.remove(&connection) {
                    // stop signaling tasks and bridge task so we can reconnect cleanly
                    let _ = shutdown.send(());
                    handle.abort();
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

    if config.proxy && config.automatic_topic_discovery {
        let receiver = watch_redis_list_items(format!("{}-proxy-topics", my_gdp_name)).await;
    }

    // Automatic topic discovery loop (polls every 5s)
    // When a new topic is detected, create a new thread to handle the topic
    let mut topic_handles = HashMap::<String, JoinHandle<()>>::new();
    loop {
        sleep(Duration::from_millis(5000)).await;

        let mut current_topics = HashMap::from_iter(
            config.ros.iter().map(|ros| (ros.topic_name.clone(), ros.clone()))
        );

        if config.automatic_topic_discovery {
            for (topic_name, topic_types) in node.get_topic_names_and_types().unwrap() {
                let has_publishers = !node.get_publishers_info_by_topic(&topic_name, true).unwrap().is_empty();
                let has_subscribers = !node.get_subscribers_info_by_topic(&topic_name, true).unwrap().is_empty();

                let action = match (has_publishers, has_subscribers) {
                    (true, true) => "proxy",
                    (true, false) => "pub",
                    (false, true) => "sub",
                    (false, false) => "noop"
                };

                current_topics.insert(topic_name.clone(), ROS {
                    topic_name,
                    action: action.into(),
                    topic_type: topic_types[0]
                });
            }
        }

        for (topic, types) in current_topics.iter() {
            if topic_handles.contains_key(topic) {
                continue;
            }

            info!("New topic discovered: {}", topic);

            let topic_name = topic.clone();
            let topic_type = types[0].clone();
            let certificate = certificate.clone();

            let handle = tokio::spawn(create_topic_network_bridge(
                topic_name.clone(),
                topic_type,
                certificate,
                my_gdp_name,
            ));

            topic_handles.insert(topic_name, handle);
        }

        topic_handles.retain(|topic_name, topic_handle| {
            if current_topics.contains_key(topic_name) {
                true
            } else {
                info!(
                    "Topic un-discovered, aborting network bridge: {}",
                    topic_name
                );
                topic_handle.abort();
                false
            }
        });
    }
}
