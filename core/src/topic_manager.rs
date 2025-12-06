use crate::network::ros::{network_to_ros_forwarder, ros_to_network_forwarder};
use crate::network::webrtc::{register_webrtc_stream, webrtc_reader_and_writer};
use crate::structs::{Connection, GDPName, gdp_name_to_string, generate_random_gdp_name, get_gdp_name_from_topic};

use std::collections::{HashMap, HashSet};
use std::str::FromStr;
use rand::Rng;
use tokio::sync::{broadcast, mpsc::unbounded_channel};
use tokio::time::Duration;
use utils::app_config::AppConfig;

use crate::db::*;

async fn handle_new_connection(
    connection: Connection, topic_name: &str, topic_type: &str, certificate: &Vec<u8>,
    my_gdp_name: GDPName, topic_gdp: GDPName,
) -> Option<broadcast::Sender<()>> {
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

    let signal_id_to_dial = if connection.publisher == my_gdp_name {
        None
    } else {
        Some(publisher_signal_id)
    };

    // Expose shutdown immediately; set up WebRTC and ROS bridging in the background.
    let (shutdown_tx, _shutdown_rx) = broadcast::channel::<()>(1);
    let shutdown_tx_for_task = shutdown_tx.clone();
    let topic_name_owned = topic_name.to_owned();
    let topic_type_owned = topic_type.to_owned();
    let certificate_owned = certificate.clone();
    let my_signal_id_owned = my_signal_id.clone();
    let is_publisher = connection.publisher == my_gdp_name;

    tokio::spawn(async move {
        let (webrtc_stream, webrtc_shutdown) =
            register_webrtc_stream(&my_signal_id_owned, signal_id_to_dial).await;

        println!("WebRTC connected!");

        let (ros_tx, ros_rx) = unbounded_channel();
        let (rtc_tx, rtc_rx) = unbounded_channel();

        let node_name = format!("ros_manager_node_{}", rand::random::<u32>());

        // Forward external shutdown to internal signaling tasks.
        let mut external_shutdown = shutdown_tx_for_task.subscribe();
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
                topic_name_owned,
                topic_type_owned,
                certificate_owned,
                rtc_tx,
            ));
        } else {
            info!("Spawning network to ros forwarder");
            tokio::spawn(network_to_ros_forwarder(
                node_name,
                topic_name_owned,
                topic_type_owned,
                certificate_owned,
                ros_rx,
            ));
        }
    });

    Some(shutdown_tx)
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
                info!("NEW CONNECTION: {}", connection_string);
                let connection = match Connection::from_str(connection_string.as_str()) {
                    Ok(c) => c,
                    Err(e) => {
                        error!("Failed to parse connection {}: {:?}", connection_string, e);
                        continue;
                    }
                };

                let topic_name_cloned = topic_name.clone();
                let topic_type_cloned = topic_type.clone();
                let certificate_cloned = certificate.clone();

                let connection_identifier = format!("{}-{}", topic_gdp, connection.to_string());

                if connection.publisher != my_gdp_name && connection.subscriber != my_gdp_name {
                    info!(
                        "Im not involved: publisher {}, subscriber {}, me {}",
                        connection.publisher, connection.subscriber, my_gdp_name
                    );
                    continue;
                }

                match handle_new_connection(
                    connection,
                    &topic_name_cloned,
                    &topic_type_cloned,
                    &certificate_cloned,
                    my_gdp_name,
                    topic_gdp,
                )
                .await
                {
                    Some(shutdown_sender) => {
                        connections.insert(connection_identifier, shutdown_sender);
                        println!("WE THE BEST MUSIC");
                    }
                    None => {
                        println!("We the worst music!");
                    }
                }
            }
            RedisListChange::Removed(connection) => {
                let connection_identifier = format!("{}-{}", topic_gdp, connection.to_string());

                if let Some(shutdown) = connections.remove(&connection_identifier) {
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
        let topic_gdp = GDPName(get_gdp_name_from_topic(
            &topic.topic_name,
            &topic.topic_type,
            &certificate,
        ));
        let publishers_topic = format!("{}-publishers", topic_gdp);
        let connections_topic = format!("{}-connections", topic_gdp);
        let proxy_topic = format!("{}-proxies", topic_gdp);

        match topic.action.as_str() {
            "pub" => {
                add_entity_to_database_as_transaction(
                    &redis_url,
                    &publishers_topic,
                    &my_gdp_name.to_string(),
                )
                .unwrap();
                info!("ADDING MYSELF TO PUBLISHERS!");
            }
            "sub" => {
                tokio::time::sleep(Duration::from_millis(2000 + (rand::thread_rng().gen::<u64>() % 1000))).await;
                let publishers = get_entity_from_database(&redis_url, &publishers_topic)
                    .unwrap()
                    .iter()
                    .map(|gdp_name_string| GDPName::from_str(gdp_name_string).unwrap())
                    .collect::<Vec<_>>();
                info!(
                    "Subscribers: looking for publisher on topic {} ({} candidates)",
                    topic.topic_name,
                    publishers.len()
                );

                let mut connections_map = HashMap::new();
                for connection in get_entity_from_database(&redis_url, &connections_topic).unwrap()
                {
                    let connection = Connection::from_str(connection.as_str()).unwrap();
                    connections_map
                        .entry(connection.publisher)
                        .or_insert(HashSet::new())
                        .insert(connection.subscriber);
                }
                info!(
                    "Existing connection fan-out: {:?}",
                    connections_map
                        .iter()
                        .map(|(k, v)| (k.to_string(), v.len()))
                        .collect::<Vec<_>>()
                );
                if let Some(least_contented_publisher) = publishers
                    .iter()
                    .filter(|publisher| {
                        connections_map
                            .get(publisher)
                            .map(|subscribers| subscribers.len())
                            .unwrap_or(0)
                            < 3
                    })
                    .min_by_key(|publisher| {
                        connections_map
                            .get(publisher)
                            .map(|subscribers| subscribers.len())
                            .unwrap_or(0)
                    })
                {
                    let connection = format!("{}-{}", least_contented_publisher, my_gdp_name);
                    info!(
                        "Connecting to least-loaded publisher {} with connection {}",
                        least_contented_publisher, connection
                    );
                    add_entity_to_database_as_transaction(
                        &redis_url,
                        &connections_topic,
                        &connection,
                    )
                    .unwrap();
                } else {
                    let proxies = get_entity_from_database(&redis_url, &proxy_topic).unwrap()
                        .iter()
                        .map(|gdpname| GDPName::from_str(gdpname).unwrap())
                        .collect::<Vec<_>>();

                    let target_publisher = publishers.first().unwrap();

                    if let Some(unused_proxy_gdp) = proxies.iter().filter(|proxy| !publishers.contains(proxy)).next() {
                        info!(
                            "No low-load publisher found; using proxy {} to reach {}",
                            unused_proxy_gdp, target_publisher
                        );
                        add_entity_to_database_as_transaction(&redis_url, &publishers_topic, &unused_proxy_gdp.to_string()).unwrap();
                        add_entity_to_database_as_transaction(&redis_url, &connections_topic, &format!("{}-{}", target_publisher, my_gdp_name)).unwrap();
                        add_entity_to_database_as_transaction(&redis_url, &connections_topic, &format!("{}-{}", unused_proxy_gdp, my_gdp_name)).unwrap();
                    } else {
                        info!(
                            "No proxy available to reach publisher {}; subscriber {} will retry later",
                            target_publisher, my_gdp_name
                        );
                    }
                }
            }
            "proxy" => {
                add_entity_to_database_as_transaction(
                    &redis_url,
                    &proxy_topic,
                    &my_gdp_name.to_string(),
                )
                .unwrap();
            }
            "noop" => {}
            action => panic!("Unknown for topic {} action: {}", topic.topic_name, action),
        };
    }

    loop {
        tokio::time::sleep(Duration::from_millis(1000)).await;
    }
}
