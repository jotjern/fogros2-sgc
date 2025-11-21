use crate::network::ros::{ros_publisher, ros_subscriber};
use crate::network::webrtc::{register_webrtc_stream, webrtc_reader_and_writer};
use crate::structs::{
    gdp_name_to_string, generate_random_gdp_name, get_gdp_name_from_topic, GDPName,
};

use async_datachannel::DataStream;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};
use std::collections::{HashMap, HashSet};
use tokio::process::Command;
use tokio::time::{sleep, Duration};
use utils::app_config::AppConfig;

use crate::db::*;
use futures::StreamExt;
use redis_async::client;

async fn watch_new_redis_list_items(
    list_key: String,
) -> UnboundedReceiver<String> {
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
        loop {
            let items = get_entity_from_database(&redis_url, &list_key)
                .unwrap_or_default();

            for item in items {
                if known_items.insert(item.clone()) {
                    let _ = tx.send(item);
                }
            }

            // Wait for a notification from the redis server
            loop {
                match stream.next().await {
                    Some(Ok(_)) => break,
                    Some(Err(e)) => error!("Error when waiting for redis updates: {}", e),
                    None => ()
                }
            }
        }
    });

    rx
}

async fn determine_topic_action(topic_name: String) -> String {
    let out = Command::new("ros2")
        .arg("topic").arg("info")
        .arg(topic_name.as_str())
        .output()
        .await
        .unwrap();

    let output_str = String::from_utf8(out.stdout).unwrap();

    if output_str.contains("Publisher count: 0") {
        "pub".into()
    } else if output_str.contains("Subscription count: 0") {
        "sub".into()
    } else {
        "noop".into()
    }
}

pub async fn ros_topic_creator(
    stream: DataStream,
    node_name: String,
    topic_name: String,
    topic_type: String,
    action: String,
    certificate: Vec<u8>,
) {
    let (ros_tx, ros_rx) = unbounded_channel();
    let (rtc_tx, rtc_rx) = unbounded_channel();

    tokio::spawn(webrtc_reader_and_writer(stream, ros_tx.clone(), rtc_rx));

    match action.as_str() {
        "sub" => {
            tokio::spawn(ros_subscriber(
                node_name,
                topic_name,
                topic_type,
                certificate,
                rtc_tx,
            ));
        }
        "pub" => {
            tokio::spawn(ros_publisher(
                node_name,
                topic_name,
                topic_type,
                certificate,
                ros_rx,
            ));
        }
        _ => panic!("unknown action"),
    };
}

async fn create_new_remote_publisher(
    topic_gdp: GDPName,
    topic_name: String,
    topic_type: String,
    certificate: Vec<u8>,
) {
    let redis_url = get_redis_url();
    let publisher_side_gdp = generate_random_gdp_name();

    let publisher_topic = format!("{}-pub", gdp_name_to_string(topic_gdp));
    let subscriber_topic = format!("{}-sub", gdp_name_to_string(topic_gdp));

    let mut changes = watch_new_redis_list_items(subscriber_topic.clone()).await;

    while let Some(new_subscriber) = changes.recv().await {
        let publisher_url = format!(
            "{},{},{}",
            gdp_name_to_string(topic_gdp),
            gdp_name_to_string(publisher_side_gdp),
            new_subscriber
        );

        add_entity_to_database_as_transaction(
            &redis_url,
            &publisher_topic,
            &publisher_url,
        )
        .expect("Cannot add publisher entry");

        let stream = register_webrtc_stream(&publisher_url, None).await;

        tokio::spawn(ros_topic_creator(
            stream,
            format!("ros_manager_node_{}", rand::random::<u32>()),
            topic_name.clone(),
            topic_type.clone(),
            "sub".into(),
            certificate.clone(),
        ));
    }
}

async fn create_new_remote_subscriber(
    topic_gdp: GDPName,
    topic_name: String,
    topic_type: String,
    certificate: Vec<u8>,
) {
    let redis_url = get_redis_url();
    let subscriber_side_gdp = generate_random_gdp_name();

    let publisher_topic = format!("{}-pub", gdp_name_to_string(topic_gdp));
    let subscriber_topic = format!("{}-sub", gdp_name_to_string(topic_gdp));

    add_entity_to_database_as_transaction(
        &redis_url,
        &subscriber_topic,
        gdp_name_to_string(subscriber_side_gdp.clone()).as_str(),
    )
    .expect("add subscriber");

    let mut changes = watch_new_redis_list_items(publisher_topic.clone()).await;

    while let Some(publisher) = changes.recv().await {
        if !publisher.ends_with(&gdp_name_to_string(subscriber_side_gdp.clone())) {
            continue;
        }

        let remote = publisher
            .split(',')
            .skip(4)
            .take(4)
            .collect::<Vec<&str>>()
            .join(",");

        let my_url = format!(
            "{},{},{}",
            gdp_name_to_string(topic_gdp),
            gdp_name_to_string(subscriber_side_gdp),
            remote
        );

        let peer_url = format!(
            "{},{},{}",
            gdp_name_to_string(topic_gdp),
            remote,
            gdp_name_to_string(subscriber_side_gdp)
        );

        sleep(Duration::from_millis(1000)).await;

        let stream = register_webrtc_stream(&my_url, Some(peer_url)).await;

        tokio::spawn(ros_topic_creator(
            stream,
            format!("ros_manager_node_{}", rand::random::<u32>()),
            topic_name.clone(),
            topic_type.clone(),
            "pub".into(),
            certificate.clone(),
        ));
    }
}

#[derive(Debug, PartialEq, Serialize, Deserialize, Clone)]
pub struct RosTopicStatus {
    pub action: String,
}

pub async fn ros_topic_manager() {
    let config = AppConfig::fetch().unwrap();
    let certificate = std::fs::read(format!(
        "./scripts/crypto/{}/{}-private.pem",
        config.crypto_name,
        config.crypto_name
    ))
    .expect("crypto file missing");

    let ctx = r2r::Context::create().unwrap();
    let node = r2r::Node::create(ctx, "ros_manager", "namespace").unwrap();

    let mut topic_status = HashMap::<String, RosTopicStatus>::new();

    loop {
        sleep(Duration::from_millis(5000)).await;

        let current = node.get_topic_names_and_types().unwrap();

        for (topic, types) in current {
            if topic_status.contains_key(&topic) {
                continue;
            }

            let tname = topic.clone();
            let ttype = types[0].clone();
            let action = determine_topic_action(tname.clone()).await;

            let gdp = GDPName(get_gdp_name_from_topic(
                &tname,
                &ttype,
                &certificate,
            ));

            topic_status.insert(
                tname.clone(),
                RosTopicStatus { action: action.clone() },
            );

            match action.as_str() {
                "sub" => {
                    tokio::spawn(create_new_remote_publisher(
                        gdp.clone(),
                        tname,
                        ttype,
                        certificate.clone(),
                    ));
                }
                "pub" => {
                    tokio::spawn(create_new_remote_subscriber(
                        gdp.clone(),
                        tname,
                        ttype,
                        certificate.clone(),
                    ));
                }
                _ => {}
            }
        }
    }
}
