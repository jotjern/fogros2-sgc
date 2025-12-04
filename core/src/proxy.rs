use crate::network::webrtc::register_webrtc_stream;

use anyhow::{anyhow, Result};
use async_datachannel::DataStream;
use futures::io::{AsyncReadExt, AsyncWriteExt};
use futures::StreamExt;
use redis::AsyncCommands;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
use tokio::sync::Mutex;
use log::{debug, error, info, warn};

const BUFFER_SIZE: usize = 1748000;

/// A single connection entry from Redis
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Connection {
    pub source: String,
    pub topic: String,
    pub destination: String,
}

impl Connection {
    pub fn parse(s: &str) -> Option<Self> {
        let parts: Vec<&str> = s.split(',').collect();
        if parts.len() == 3 {
            Some(Connection {
                source: parts[0].to_string(),
                topic: parts[1].to_string(),
                destination: parts[2].to_string(),
            })
        } else {
            None
        }
    }
}

/// Message passed between internal channels
#[derive(Debug, Clone)]
pub struct TopicMessage {
    pub topic: String,
    pub payload: Vec<u8>,
}

/// Handle to an active peer connection
struct PeerHandle {
    tx: UnboundedSender<TopicMessage>,
    task: tokio::task::JoinHandle<()>,
}

/// Main proxy state
pub struct Proxy {
    id: String,
    redis_url: String,

    /// WebRTC connections to other proxies; used to send messages to them
    peers: HashMap<String, PeerHandle>,

    /// Routing: (source_peer, topic) -> [destination_peers]
    routes: Arc<Mutex<HashMap<(String, String), Vec<String>>>>,

    /// Sender for incoming messages from all peers
    incoming_tx: UnboundedSender<(String, TopicMessage)>,
    incoming_rx: Option<UnboundedReceiver<(String, TopicMessage)>>,
}

impl Proxy {
    pub async fn new(id: String, redis_addr: Option<String>) -> Result<Self> {
        let redis_url = format!(
            "redis://{}",
            redis_addr.unwrap_or_else(|| "localhost:8002".to_string())
        );

        let (incoming_tx, incoming_rx) = unbounded_channel();

        Ok(Proxy {
            id,
            redis_url,
            peers: HashMap::new(),
            routes: Arc::new(Mutex::new(HashMap::new())),
            incoming_tx,
            incoming_rx: Some(incoming_rx),
        })
    }

    pub async fn run(&mut self) -> Result<()> {
        info!("Proxy {} starting...", self.id);

        // 1. Register self in Redis
        debug!("Step 1: Registering in Redis...");
        self.register_in_redis().await?;

        // 2. Enable keyspace notifications
        debug!("Step 2: Enabling keyspace notifications...");
        self.enable_keyspace_notifications().await?;

        // 3. Initial sync
        debug!("Step 3: Syncing connections...");
        self.sync_connections().await?;

        // 4. Take ownership of incoming_rx for the routing task
        debug!("Step 4: Setting up routing infrastructure...");
        let incoming_rx = self
            .incoming_rx
            .take()
            .ok_or_else(|| anyhow!("incoming_rx already taken"))?;
        let routes = self.routes.clone();
        let peers_for_routing: Arc<Mutex<HashMap<String, UnboundedSender<TopicMessage>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let peers_for_routing_clone = peers_for_routing.clone();

        // 5. Spawn message routing task
        debug!("Step 5: Spawning message routing task...");
        let routing_task = tokio::spawn(async move {
            Self::route_messages(incoming_rx, routes, peers_for_routing_clone).await;
        });

        // 6. Watch Redis for connection changes
        debug!("Step 6: Starting Redis watcher...");
        let _watch_task = self.watch_redis_changes(peers_for_routing).await;

        info!("Proxy {} fully initialized, waiting for messages...", self.id);

        // 7. Wait for shutdown signal
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                info!("Received shutdown signal");
            }
            _ = routing_task => {
                error!("Routing task ended unexpectedly");
            }
        }

        // 8. Cleanup
        self.unregister_from_redis().await?;

        Ok(())
    }

    async fn register_in_redis(&self) -> Result<()> {
        let client = redis::Client::open(self.redis_url.as_str())?;
        let mut con = client.get_multiplexed_async_connection().await?;

        let _: () = con.rpush("proxies", &self.id).await?;
        info!("Registered proxy {} in Redis", self.id);

        Ok(())
    }

    async fn unregister_from_redis(&self) -> Result<()> {
        let client = redis::Client::open(self.redis_url.as_str())?;
        let mut con = client.get_multiplexed_async_connection().await?;

        let _: () = con.lrem("proxies", 0, &self.id).await?;
        info!("Unregistered proxy {} from Redis", self.id);

        Ok(())
    }

    async fn enable_keyspace_notifications(&self) -> Result<()> {
        let client = redis::Client::open(self.redis_url.as_str())?;
        let mut con = client.get_multiplexed_async_connection().await?;

        redis::cmd("CONFIG")
            .arg("SET")
            .arg("notify-keyspace-events")
            .arg("KEA")
            .query_async::<_, ()>(&mut con)
            .await?;

        info!("Enabled Redis keyspace notifications");
        Ok(())
    }

    async fn fetch_my_connections(&self) -> Result<Vec<Connection>> {
        let client = redis::Client::open(self.redis_url.as_str())?;
        let mut con = client.get_multiplexed_async_connection().await?;

        let all: Vec<String> = con.lrange("connections", 0, -1).await?;
        debug!("All connections in Redis: {:?}", all);

        let my_connections: Vec<Connection> = all
            .iter()
            .filter_map(|s| Connection::parse(s))
            .filter(|c| c.source == self.id || c.destination == self.id)
            .collect();

        info!(
            "Fetched {} connections involving {} (out of {} total)",
            my_connections.len(),
            self.id,
            all.len()
        );
        for conn in &my_connections {
            debug!("  {:?}", conn);
        }
        Ok(my_connections)
    }

    async fn sync_connections(&mut self) -> Result<()> {
        let my_connections = self.fetch_my_connections().await?;

        // Determine needed peers
        let needed_peers: HashSet<String> = my_connections
            .iter()
            .flat_map(|c| {
                let mut peers = vec![];
                if c.source == self.id {
                    peers.push(c.destination.clone());
                }
                if c.destination == self.id {
                    peers.push(c.source.clone());
                }
                peers
            })
            .collect();

        debug!("Needed peers: {:?}", needed_peers);
        debug!("Current peers: {:?}", self.peers.keys().collect::<Vec<_>>());

        // Close connections to peers no longer needed
        let current_peers: HashSet<String> = self.peers.keys().cloned().collect();
        for peer_id in current_peers.difference(&needed_peers) {
            info!("Closing connection to peer {}", peer_id);
            if let Some(handle) = self.peers.remove(peer_id) {
                handle.task.abort();
            }
        }

        // Open connections to new peers
        for peer_id in needed_peers.difference(&current_peers) {
            info!("Opening connection to peer {}", peer_id);
            if let Err(e) = self.open_peer_connection(peer_id.clone()).await {
                error!("Failed to connect to peer {}: {}", peer_id, e);
            }
        }

        // Rebuild routing table
        let mut new_routes: HashMap<(String, String), Vec<String>> = HashMap::new();

        for conn in &my_connections {
            if conn.destination == self.id {
                // We receive from conn.source on conn.topic
                // Find where to forward: all connections where we are source with same topic
                let destinations: Vec<String> = my_connections
                    .iter()
                    .filter(|c| c.source == self.id && c.topic == conn.topic)
                    .map(|c| c.destination.clone())
                    .collect();

                if !destinations.is_empty() {
                    debug!(
                        "Route: ({}, {}) -> {:?}",
                        conn.source, conn.topic, destinations
                    );
                    new_routes.insert((conn.source.clone(), conn.topic.clone()), destinations);
                }
            }
        }

        *self.routes.lock().await = new_routes;
        info!(
            "Updated routing table with {} entries",
            self.routes.lock().await.len()
        );

        Ok(())
    }

    async fn open_peer_connection(&mut self, peer_id: String) -> Result<()> {
        let i_initiate = self.id < peer_id;

        let my_signaling_id = format!("{}-{}", self.id, peer_id);
        let peer_signaling_id = format!("{}-{}", peer_id, self.id);

        info!(
            "Establishing WebRTC connection with {}, I {} initiate (my_id={}, peer_id={})",
            peer_id,
            if i_initiate { "will" } else { "will not" },
            my_signaling_id,
            peer_signaling_id
        );

        debug!("Calling register_webrtc_stream...");
        let stream = if i_initiate {
            register_webrtc_stream(&my_signaling_id, Some(peer_signaling_id)).await
        } else {
            register_webrtc_stream(&my_signaling_id, None).await
        };
        info!("WebRTC stream established with peer {}", peer_id);

        let (tx, rx) = unbounded_channel::<TopicMessage>();
        let incoming_tx = self.incoming_tx.clone();
        let peer_id_clone = peer_id.clone();

        debug!("Spawning peer relay task for {}", peer_id);
        let task = tokio::spawn(async move {
            info!("Peer relay task started for {}", peer_id_clone);
            if let Err(e) = peer_relay_task(stream, peer_id_clone.clone(), rx, incoming_tx).await {
                error!(
                    "Peer relay task for {} ended with error: {}",
                    peer_id_clone, e
                );
            }
            info!("Peer relay task ended for {}", peer_id_clone);
        });

        self.peers.insert(peer_id.clone(), PeerHandle { tx, task });
        info!("Peer {} added to peers map", peer_id);

        Ok(())
    }

    async fn watch_redis_changes(
        &mut self,
        peers_for_routing: Arc<Mutex<HashMap<String, UnboundedSender<TopicMessage>>>>,
    ) -> Result<()> {
        debug!("Connecting to Redis for pubsub...");
        let client = redis::Client::open(self.redis_url.as_str())?;
        let mut pubsub = client.get_async_connection().await?.into_pubsub();

        pubsub.psubscribe("__keyspace@0__:connections").await?;
        info!("Subscribed to Redis keyspace notifications for 'connections'");

        let mut stream = pubsub.on_message();
        debug!("Waiting for Redis keyspace events...");

        while let Some(msg) = stream.next().await {
            let payload: String = msg.get_payload().unwrap_or_default();
            info!(
                "Redis keyspace event: channel={}, payload={}",
                msg.get_channel_name(),
                payload
            );

            if let Err(e) = self.sync_connections().await {
                error!("Failed to sync connections: {}", e);
            }

            // Update the peers_for_routing map
            let mut pfr = peers_for_routing.lock().await;
            pfr.clear();
            for (peer_id, handle) in &self.peers {
                pfr.insert(peer_id.clone(), handle.tx.clone());
            }
            debug!("Updated peers_for_routing with {} peers", pfr.len());
        }

        warn!("Redis pubsub stream ended unexpectedly");
        Ok(())
    }

    async fn route_messages(
        mut incoming_rx: UnboundedReceiver<(String, TopicMessage)>,
        routes: Arc<Mutex<HashMap<(String, String), Vec<String>>>>,
        peers: Arc<Mutex<HashMap<String, UnboundedSender<TopicMessage>>>>,
    ) {
        info!("Message router started, waiting for incoming messages...");
        let mut msg_count = 0u64;

        while let Some((from_peer, msg)) = incoming_rx.recv().await {
            msg_count += 1;
            let key = (from_peer.clone(), msg.topic.clone());

            debug!(
                "[msg #{}] Received {} bytes from {} on topic '{}'",
                msg_count,
                msg.payload.len(),
                from_peer,
                msg.topic
            );

            let routes_guard = routes.lock().await;
            if let Some(destinations) = routes_guard.get(&key) {
                let peers_guard = peers.lock().await;

                for dest in destinations {
                    if let Some(tx) = peers_guard.get(dest) {
                        debug!(
                            "[msg #{}] Routing {} -> {} on topic '{}'",
                            msg_count, from_peer, dest, msg.topic
                        );
                        if tx.send(msg.clone()).is_err() {
                            warn!("Failed to send to peer {}", dest);
                        }
                    } else {
                        warn!(
                            "[msg #{}] Destination {} not in peers map",
                            msg_count, dest
                        );
                    }
                }
            } else {
                debug!(
                    "[msg #{}] No route for ({}, '{}'). Routes: {:?}",
                    msg_count, from_peer, msg.topic, routes_guard.keys().collect::<Vec<_>>()
                );
            }
        }
        warn!("Message router: incoming_rx channel closed");
    }
}

/// Bidirectional relay between WebRTC stream and internal channels
///
/// Data flow:
///   [Proxy] --outgoing_rx--> [This Task] --WebRTC--> [Remote Peer]
///   [Proxy] <--incoming_tx-- [This Task] <--WebRTC-- [Remote Peer]
async fn peer_relay_task(
    mut stream: DataStream,
    peer_id: String,
    // Messages TO SEND to the remote peer (Proxy gives us messages here, we write to WebRTC)
    mut outgoing_rx: UnboundedReceiver<TopicMessage>,
    // Messages RECEIVED from the remote peer (we read from WebRTC, send to Proxy via this channel)
    incoming_tx: UnboundedSender<(String, TopicMessage)>,
) -> Result<()> {
    let mut buf = vec![0u8; BUFFER_SIZE];
    let mut recv_count = 0u64;
    let mut send_count = 0u64;

    debug!("[{}] Relay task running, waiting for data...", peer_id);

    loop {
        tokio::select! {
            // Receive from WebRTC
            result = stream.read(&mut buf) => {
                match result {
                    Ok(0) => {
                        info!("[{}] WebRTC stream closed (EOF)", peer_id);
                        break;
                    }
                    Ok(n) => {
                        recv_count += 1;
                        debug!("[{}] Received {} bytes (packet #{})", peer_id, n, recv_count);

                        let data = &buf[..n];

                        if let Some((header, payload)) = parse_gdp_packet(data) {
                            debug!(
                                "[{}] Parsed packet: topic='{}', payload_len={}",
                                peer_id, header.topic, payload.len()
                            );

                            let msg = TopicMessage {
                                topic: header.topic,
                                payload: payload.to_vec(),
                            };

                            if incoming_tx.send((peer_id.clone(), msg)).is_err() {
                                error!("[{}] Failed to send to router (channel closed)", peer_id);
                                break;
                            }
                        } else {
                            warn!(
                                "[{}] Failed to parse packet ({} bytes): {:?}",
                                peer_id, n, &data[..n.min(100)]
                            );
                        }
                    }
                    Err(e) => {
                        error!("[{}] WebRTC read error: {}", peer_id, e);
                        break;
                    }
                }
            }

            // Send to WebRTC
            Some(msg) = outgoing_rx.recv() => {
                send_count += 1;
                let packet = serialize_gdp_packet(&msg);
                debug!(
                    "[{}] Sending packet #{}: topic='{}', {} bytes",
                    peer_id, send_count, msg.topic, packet.len()
                );

                if let Err(e) = stream.write_all(&packet).await {
                    error!("[{}] WebRTC write error: {}", peer_id, e);
                    break;
                }
            }
        }
    }

    info!(
        "[{}] Relay task ending. Stats: {} received, {} sent",
        peer_id, recv_count, send_count
    );
    Ok(())
}

/// Header for proxy packets (simpler than full GDPHeaderInTransit)
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct ProxyHeader {
    topic: String,
    length: usize,
}

fn parse_gdp_packet(data: &[u8]) -> Option<(ProxyHeader, Vec<u8>)> {
    // Find null byte separator
    let null_pos = data.iter().position(|&b| b == 0)?;

    let header_bytes = &data[..null_pos];
    let payload = &data[null_pos + 1..];

    let header_str = std::str::from_utf8(header_bytes).ok()?;
    let header: ProxyHeader = serde_json::from_str(header_str).ok()?;

    let length = header.length;  // Copy the length before moving header
    if payload.len() >= length {
        Some((header, payload[..length].to_vec()))
    } else {
        None
    }
}

fn serialize_gdp_packet(msg: &TopicMessage) -> Vec<u8> {
    let header = ProxyHeader {
        topic: msg.topic.clone(),
        length: msg.payload.len(),
    };

    let mut packet = serde_json::to_vec(&header).unwrap();
    packet.push(0); // null separator
    packet.extend_from_slice(&msg.payload);

    packet
}