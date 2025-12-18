//! ROS2 <-> WebRTC bridge.
//!
//! Provides two forwarders:
//! - `ros_to_network_forwarder`: Subscribes to ROS topic, sends to WebRTC
//! - `network_to_ros_forwarder`: Receives from WebRTC, publishes to ROS topic

use crate::db::get_redis_url;
use crate::pipeline::construct_gdp_forward_from_bytes;
use crate::structs::get_gdp_name_from_topic;
use crate::structs::{GDPName, GDPPacket, GdpAction, Packet};
use base64;
use futures::stream::StreamExt;
use log::{error, info, warn};
use redis::Commands;
use serde_json::json;

#[cfg(feature = "ros")]
use r2r::QosProfile;

use tokio::sync::mpsc::UnboundedReceiver;
use tokio::sync::mpsc::UnboundedSender;
use tokio::task;

/// Publishes debug signals to Redis for dashboard visualization.
#[cfg(feature = "ros")]
fn fire_debug_signal(node: &GDPName, topic: &str, direction: &str, payload: &[u8]) {
    let topic = topic.to_string();
    let node_id = node.to_string();
    let direction = direction.to_string();
    let content = match std::str::from_utf8(payload) {
        Ok(s) => s.to_string(),
        Err(_) => base64::encode(payload),
    };

    tokio::spawn(async move {
        let redis_url = get_redis_url();
        let msg = json!({
            "node": node_id,
            "topic": topic,
            "direction": direction,
            "content": content,
        })
        .to_string();

        let _ = tokio::task::spawn_blocking(move || {
            if let Ok(client) = redis::Client::open(redis_url) {
                if let Ok(mut conn) = client.get_connection() {
                    let _: Result<(), _> = redis::cmd("PUBLISH")
                        .arg("debug-messages")
                        .arg(&msg)
                        .query(&mut conn);
                } else {
                    info!("debug publish: failed to get redis connection");
                }
            } else {
                info!("debug publish: failed to open redis client");
            }
        })
        .await;
    });
}

// Publishes messages to local ROS (receives from WebRTC)
#[cfg(feature = "ros")]
pub async fn network_to_ros_forwarder(
    node_name: String, topic_name: String, topic_type: String, certificate: Vec<u8>,
    my_gdp_name: GDPName, mut m_rx: UnboundedReceiver<GDPPacket>,
) {
    use crate::db::mark_node_received_data;
    
    let node_gdp_name = GDPName(get_gdp_name_from_topic(
        &node_name,
        &topic_type,
        &certificate,
    ));
    info!("ROS {} takes gdp name {:?}", node_name, node_gdp_name);

    let topic_gdp_name = GDPName(get_gdp_name_from_topic(
        &topic_name,
        &topic_type,
        &certificate,
    ));
    info!("topic {} takes gdp name {:?}", topic_name, topic_gdp_name);

    let ctx = r2r::Context::create().expect("context creation failure");
    let mut node = r2r::Node::create(ctx, &node_name, "namespace").expect("node creation failure");
    let publisher = node
        .create_publisher_untyped(&topic_name, &topic_type, QosProfile::default())
        .expect("publisher creation failure");

    let _handle = task::spawn_blocking(move || loop {
        node.spin_once(std::time::Duration::from_millis(10));
        std::thread::sleep(std::time::Duration::from_millis(100));
    });

    // Track if we've already marked this node as having received data (to avoid spamming Redis)
    let mut marked_received = false;

    while let Some(pkt_to_forward) = m_rx.recv().await {
        if pkt_to_forward.action == GdpAction::Forward {
            info!("new payload to publish ");
            if pkt_to_forward.gdpname == topic_gdp_name {
                if let Some(payload) = pkt_to_forward.get_byte_payload() {
                    // Mark this node as having received data (only once per session)
                    if !marked_received {
                        mark_node_received_data(my_gdp_name);
                        marked_received = true;
                        info!("Marked node {} as having received data", my_gdp_name);
                    }
                    
                    fire_debug_signal(&my_gdp_name, &topic_name, "network_receive", payload);
                    if let Err(e) = publisher.publish(payload.clone()) {
                        error!("Failed to publish to ROS topic {}: {:?}", topic_name, e);
                    } else {
                        fire_debug_signal(&my_gdp_name, &topic_name, "ros_publish", payload);
                    }
                } else {
                    warn!("Received packet with no payload for topic {}", topic_name);
                }
            } else {
                info!(
                    "{:?} received a packet for name {:?}",
                    pkt_to_forward.gdpname, topic_gdp_name
                );
            }
        }
    }
}

// Subscribes to local ROS and sends to WebRTC
#[cfg(feature = "ros")]
pub async fn ros_to_network_forwarder(
    node_name: String, topic_name: String, topic_type: String, certificate: Vec<u8>,
    m_tx: UnboundedSender<GDPPacket>,
) {
    let node_gdp_name = GDPName(get_gdp_name_from_topic(
        &node_name,
        &topic_type,
        &certificate,
    ));
    info!("ROS {} takes gdp name {:?}", node_name, node_gdp_name);

    let topic_gdp_name = GDPName(get_gdp_name_from_topic(
        &topic_name,
        &topic_type,
        &certificate,
    ));
    info!("topic {} takes gdp name {:?}", topic_name, topic_gdp_name);

    let ctx = r2r::Context::create().expect("context creation failure");
    let mut node = r2r::Node::create(ctx, &node_name, "namespace").expect("node creation failure");
    info!("Subscribing to {}", topic_name);
    let mut subscriber = node
        .subscribe_untyped(&topic_name, &topic_type, QosProfile::default())
        .expect("topic subscribing failure");

    let _handle = task::spawn_blocking(move || loop {
        node.spin_once(std::time::Duration::from_millis(10));
        std::thread::sleep(std::time::Duration::from_millis(100));
    });

    while let Some(packet) = subscriber.next().await {
        info!("received a packet {:?}", packet);
        let ros_msg = packet;
        fire_debug_signal(&node_gdp_name, &topic_name, "ros_receive", &ros_msg);
        let packet = construct_gdp_forward_from_bytes(topic_gdp_name, node_gdp_name, ros_msg.clone());
        if m_tx.send(packet).is_err() {
            break;
        }
        fire_debug_signal(&node_gdp_name, &topic_name, "network_send", &ros_msg);
    }
}
