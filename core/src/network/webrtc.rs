//! WebRTC data channel for encrypted peer-to-peer communication.
//!
//! Uses a signaling server to exchange SDP offers/answers and ICE candidates,
//! then establishes direct WebRTC data channels for GDP packet transfer.

use std::collections::{HashSet, VecDeque};
use std::sync::Arc;
use std::sync::OnceLock;
use std::time::Duration;

use crate::pipeline::construct_gdp_forward_from_bytes;
use crate::structs::GDPHeaderInTransit;
use crate::structs::{generate_random_gdp_name, GDPName};
use crate::structs::{GDPPacket, GdpAction, Packet};

use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

use async_datachannel::{DataStream, Message, PeerConnection, RtcConfig};
use async_tungstenite::{tokio::connect_async, tungstenite};
use futures::{
    channel::mpsc,
    io::{AsyncReadExt, AsyncWriteExt},
    SinkExt, StreamExt,
};
use log::{error, info, warn};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;
use utils::app_config::AppConfig;

const RECEIVE_BUFFER_SIZE: usize = 1_748_000; // ~1.7MB for large messages
const MAX_PARSE_RETRIES: usize = 5;

static ACTIVE_SIGNAL_IDS: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();

fn active_ids() -> &'static Mutex<HashSet<String>> {
    ACTIVE_SIGNAL_IDS.get_or_init(|| Mutex::new(HashSet::new()))
}

pub struct WebRtcGuard {
    id: String,
}

impl Drop for WebRtcGuard {
    fn drop(&mut self) {
        active_ids().lock().remove(&self.id);
    }
}

#[derive(Debug)]
pub enum WebRtcError {
    ConfigError(String),
    PeerConnectionFailed(String),
    SignalingConnectionFailed(String),
    DataChannelFailed(String),
    DuplicateConnection(String),
}

impl std::fmt::Display for WebRtcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WebRtcError::ConfigError(e) => write!(f, "Config error: {}", e),
            WebRtcError::PeerConnectionFailed(e) => write!(f, "PeerConnection failed: {}", e),
            WebRtcError::SignalingConnectionFailed(e) => write!(f, "Signaling connection failed: {}", e),
            WebRtcError::DataChannelFailed(e) => write!(f, "Data channel failed: {}", e),
            WebRtcError::DuplicateConnection(id) => write!(f, "Duplicate connection attempt for {}", id),
        }
    }
}

impl std::error::Error for WebRtcError {}

/// Reassembles GDP packets from a byte stream.
/// Wire format: JSON header + '\0' + binary payload
pub struct PacketParser {
    buffer: Vec<u8>,
    pending_header: Option<GDPHeaderInTransit>,
    parse_retries: usize,
}

impl PacketParser {
    pub fn new() -> Self {
        Self {
            buffer: Vec::new(),
            pending_header: None,
            parse_retries: 0,
        }
    }

    /// Feed new data, returns any complete packets.
    pub fn feed(&mut self, data: &[u8]) -> Vec<(GDPHeaderInTransit, Vec<u8>)> {
        self.buffer.extend_from_slice(data);
        let mut complete_packets = Vec::new();

        loop {
            match self.pending_header.take() {
                Some(header) => {
                    // Have header, waiting for payload
                    if self.buffer.len() >= header.length {
                        let payload: Vec<u8> = self.buffer.drain(..header.length).collect();
                        complete_packets.push((header, payload));
                        self.parse_retries = 0;
                    } else {
                        self.pending_header = Some(header);
                        break;
                    }
                }
                None => {
                    // Looking for header (delimited by null byte)
                    let Some(null_pos) = self.buffer.iter().position(|&b| b == 0) else {
                        break;
                    };

                    let header_bytes: Vec<u8> = self.buffer.drain(..null_pos).collect();
                    self.buffer.drain(..1); // Remove null byte

                    match std::str::from_utf8(&header_bytes)
                        .ok()
                        .and_then(|s| serde_json::from_str::<GDPHeaderInTransit>(s).ok())
                    {
                        Some(header) => {
                            self.pending_header = Some(header);
                            self.parse_retries = 0;
                        }
                        None => {
                            self.parse_retries += 1;
                            if self.parse_retries > MAX_PARSE_RETRIES {
                                warn!("Failed to parse header after {} attempts, clearing buffer", MAX_PARSE_RETRIES);
                                self.buffer.clear();
                                self.parse_retries = 0;
                            }
                            break;
                        }
                    }
                }
            }
        }

        complete_packets
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct SignalingMessage {
    id: String,
    payload: Message,
}

/// Establish WebRTC data channel via signaling server.
/// Returns the data stream and a shutdown sender for cleanup.
pub async fn register_webrtc_stream(
    my_id: &str,
    peer_to_dial: Option<String>,
) -> Result<(DataStream, broadcast::Sender<()>, WebRtcGuard), WebRtcError> {
    let my_id = my_id.to_string();
    {
        let mut set = active_ids().lock();
        if set.contains(&my_id) {
            return Err(WebRtcError::DuplicateConnection(my_id));
        }
        set.insert(my_id.clone());
    }
    let guard = WebRtcGuard { id: my_id.clone() };
    let is_initiator = peer_to_dial.is_some();
    
    info!("[WebRTC] Setup for {}: mode={}", my_id, if is_initiator { "initiator" } else { "responder" });

    let config = AppConfig::fetch()
        .map_err(|e| WebRtcError::ConfigError(e.to_string()))?;

    // STUN server for NAT traversal
    let ice_servers = vec!["stun:stun.l.google.com:19302"];
    let conf = RtcConfig::new(&ice_servers);

    // Channels for SDP/ICE exchange
    let (tx_sig_outbound, mut rx_sig_outbound) = mpsc::channel(32);
    let (mut tx_sig_inbound, rx_sig_inbound) = mpsc::channel(32);

    let listener = PeerConnection::new(&conf, (tx_sig_outbound, rx_sig_inbound))
        .map_err(|e| WebRtcError::PeerConnectionFailed(format!("{:?}", e)))?;

    let (shutdown_tx, shutdown_rx) = broadcast::channel::<()>(1);

    // Connect to signaling server
    let signaling_uri = format!("{}/{}", config.signaling_server_address, my_id);
    let (mut ws_write, mut ws_read) = connect_async(&signaling_uri)
        .await
        .map_err(|e| WebRtcError::SignalingConnectionFailed(format!("{:?}", e)))?
        .0
        .split();

    info!("[WebRTC] Connected to signaling server: {}", signaling_uri);

    let other_peer = Arc::new(Mutex::new(peer_to_dial.clone()));
    let other_peer_writer = other_peer.clone();

    // Task: relay outbound signaling messages to WebSocket
    let mut shutdown_rx_w = shutdown_rx.resubscribe();
    let my_id_w = my_id.clone();
    tokio::spawn(async move {
        let mut pending: VecDeque<Message> = VecDeque::new();
        loop {
            tokio::select! {
                _ = shutdown_rx_w.recv() => break,
                Some(m) = rx_sig_outbound.next() => pending.push_back(m),
                else => break,
            }
            
            let Some(peer_id) = other_peer_writer.lock().clone() else {
                continue;
            };
            
            while let Some(msg) = pending.pop_front() {
                let m = SignalingMessage { payload: msg, id: peer_id.clone() };
                if let Ok(s) = serde_json::to_string(&m) {
                    if ws_write.send(tungstenite::Message::text(s)).await.is_err() {
                        error!("[WebRTC] Signaling write failed for {}", my_id_w);
                        return;
                    }
                }
            }
        }
    });

    // Task: relay inbound signaling messages from WebSocket
    let mut shutdown_rx_r = shutdown_rx.resubscribe();
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = shutdown_rx_r.recv() => break,
                msg = ws_read.next() => {
                    let Some(Ok(m)) = msg else { break };
                    
                    let val = match m {
                        tungstenite::Message::Text(t) => serde_json::from_str(&t).ok(),
                        tungstenite::Message::Binary(b) => serde_json::from_slice(&b).ok(),
                        tungstenite::Message::Close(_) => break,
                        _ => continue,
                    };
                    
                    if let Some(c) = val.and_then(|v: serde_json::Value| {
                        serde_json::from_value::<SignalingMessage>(v).ok()
                    }) {
                        other_peer.lock().replace(c.id);
                        let _ = tx_sig_inbound.send(c.payload).await;
                    }
                }
            }
        }
    });

    // Establish data channel (initiator dials, responder accepts)
    let stream = if let Some(peer_id) = peer_to_dial {
        info!("[WebRTC] Dialing peer: {}", peer_id);
        listener
            .dial("data")
            .await
            .map_err(|e| WebRtcError::DataChannelFailed(format!("dial: {:?}", e)))?
    } else {
        info!("[WebRTC] Waiting for incoming connection...");
        // Small delay to allow signaling messages to queue before accept() processes them
        tokio::time::sleep(Duration::from_millis(100)).await;
        listener
            .accept()
            .await
            .map_err(|e| WebRtcError::DataChannelFailed(format!("accept: {:?}", e)))?
    };

    info!("[WebRTC] Data channel established for {}", my_id);
    Ok((stream, shutdown_tx, guard))
}

/// Bidirectional forwarding between WebRTC stream and ROS channels.
pub async fn webrtc_reader_and_writer(
    mut stream: DataStream,
    ros_tx: UnboundedSender<GDPPacket>,
    mut rtc_rx: UnboundedReceiver<GDPPacket>,
    _guard: WebRtcGuard,
) {
    let thread_name = generate_random_gdp_name();
    let mut parser = PacketParser::new();
    let mut outbound_closed = false;
    let mut stats = (0u64, 0u64); // (received, sent)

    loop {
        let mut buf = vec![0u8; RECEIVE_BUFFER_SIZE];

        tokio::select! {
            // WebRTC -> ROS
            read_result = stream.read(&mut buf) => {
                let n = match read_result {
                    Ok(0) => break,
                    Ok(n) => n,
                    Err(e) => {
                        error!("[WebRTC] Read error: {}", e);
                        break;
                    }
                };

                for (header, payload) in parser.feed(&buf[..n]) {
                    if header.action == GdpAction::Forward {
                        stats.0 += 1;
                        let packet = construct_gdp_forward_from_bytes(header.destination, thread_name, payload);
                        if ros_tx.send(packet).is_err() {
                            error!("[WebRTC] ROS channel closed");
                        }
                    }
                }
            }

            // ROS -> WebRTC
            pkt = rtc_rx.recv(), if !outbound_closed => {
                let Some(pkt) = pkt else {
                    outbound_closed = true;
                    continue;
                };

                let header = pkt.get_header();
                let mut data = serde_json::to_vec(&header).unwrap_or_default();
                data.push(0); // Null delimiter
                
                if let Some(payload) = &pkt.payload {
                    data.extend_from_slice(payload);
                }
                if let Some(record) = &pkt.name_record {
                    if let Ok(record_bytes) = serde_json::to_vec(record) {
                        data.extend_from_slice(&record_bytes);
                    }
                }

                if stream.write_all(&data).await.is_err() {
                    error!("[WebRTC] Write error");
                    break;
                }
                stats.1 += 1;
            }
        }
    }

    info!("[WebRTC] Connection closed. Stats: received={}, sent={}", stats.0, stats.1);
}
