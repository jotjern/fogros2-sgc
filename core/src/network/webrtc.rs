use std::collections::VecDeque;
use std::fs::File;
use std::sync::Arc;
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
use tokio::sync::broadcast;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use log::{error, info, warn};
use utils::app_config::AppConfig;

// Buffer size for receiving WebRTC data chunks (1.7MB)
const UDP_BUFFER_SIZE: usize = 1748000;
const MAX_RESET_ATTEMPTS: usize = 5;

/// Parses GDP packets from a byte buffer using null-byte delimited JSON headers.
///
/// Packet format: `[JSON Header]\0[Binary Payload]`
///
/// # Returns
/// - A vector of complete (header, payload) pairs
/// - An optional incomplete (header, payload) if more data is needed
///
/// # Behavior
/// - Handles multiple complete packets in one buffer
/// - Returns partial packet state when data is incomplete
/// - Uses Noop header to signal unparseable data
pub fn parse_header_payload_pairs(
    mut buffer: Vec<u8>,
) -> (
    Vec<(GDPHeaderInTransit, Vec<u8>)>,
    Option<(GDPHeaderInTransit, Vec<u8>)>,
) {
    let mut header_payload_pairs: Vec<(GDPHeaderInTransit, Vec<u8>)> = Vec::new();
    
    let default_gdp_header: GDPHeaderInTransit = GDPHeaderInTransit {
        action: GdpAction::Noop,
        destination: GDPName([0u8, 0, 0, 0]),
        length: 0, // doesn't have any payload
    };
    
    if buffer.is_empty() {
        return (header_payload_pairs, None);
    }
    
    loop {
        // Split buffer at first null byte: [header]\0[payload + rest]
        let header_and_remaining = buffer.splitn(2, |c| c == &0).collect::<Vec<_>>();
        let header_buf = header_and_remaining[0];
        let header: &str = match std::str::from_utf8(header_buf) {
            Ok(s) => s,
            Err(e) => {
                error!("Failed to parse header as UTF-8: {}", e);
                continue;
            }
        };
        info!("received header json string: {:?}", header);
        
        // Try parsing JSON header
        let gdp_header = match serde_json::from_str::<GDPHeaderInTransit>(header) {
            Ok(h) => h,
            Err(e) => {
                warn!("Header parsing failed (may be incomplete): {}, returning remaining buffer", e);
                return (
                    header_payload_pairs,
                    Some((default_gdp_header, header_buf.to_vec())),
                );
            }
        };
        let remaining = header_and_remaining[1];

        // Check if we have enough data for the payload
        if gdp_header.length > remaining.len() {
            // Incomplete payload - need more data
            return (header_payload_pairs, Some((gdp_header, remaining.to_vec())));
        } else if gdp_header.length == remaining.len() {
            // If the payload is complete, return the pair
            // Exact match - this is the last packet
            header_payload_pairs.push((gdp_header, remaining.to_vec()));
            return (header_payload_pairs, None);
        } else {
            // If the payload is not complete, return the remaining
            // Buffer contains additional packets - extract payload and continue
            header_payload_pairs.push((gdp_header, remaining[..gdp_header.length].to_vec()));
            buffer = remaining[gdp_header.length..].to_vec();
        }
    }
}

// ============================================================================
// WebRTC Connection Establishment
// ============================================================================

// Works with the signalling server from https://github.com/paullouisageneau/libdatachannel/tree/master/examples/signaling-server-rust
// Start two shells
// 1. RUST_LOG=debug cargo run --example smoke -- ws://127.0.0.1:8000 other_peer
// 2. RUST_LOG=debug cargo run --example smoke -- ws://127.0.0.1:8000 initiator other_peer

#[derive(Debug, Serialize, Deserialize)]
struct SignalingMessage {
    id: String,      // Target peer ID (the id of the peer this message is supposed for)
    payload: Message, // WebRTC signaling data (SDP offer/answer, ICE candidates)
}

/// Establishes a WebRTC data channel connection via signaling server.
///
/// # Process
/// 1. Connects to signaling server via WebSocket
/// 2. Exchanges ICE candidates and SDP offers/answers through signaling server
/// 3. Establishes direct peer-to-peer WebRTC data channel
///
/// # Arguments
/// - `my_id`: This peer's identifier
/// - `peer_to_dial`: If Some, initiates connection; if None, waits for incoming connection
///
/// # Returns
/// An established WebRTC DataStream for bidirectional communication, plus a shutdown signal
/// that cleanly stops signaling tasks (useful when reconnecting with the same ID).
pub async fn register_webrtc_stream(
    my_id: &str,
    peer_to_dial: Option<String>,
) -> (DataStream, broadcast::Sender<()>) {
    info!("[WebRTC] Starting WebRTC connection setup for signal_id: {}", my_id);
    info!("[WebRTC] Connection mode: {}", if peer_to_dial.is_some() { "initiator (dialing)" } else { "responder (accepting)" });
    
    // Own my_id so it can be moved into spawned tasks safely
    let my_id = my_id.to_string();
    let config = AppConfig::fetch().unwrap_or_else(|e| {
        error!("[WebRTC] Failed to fetch config: {}", e);
        panic!("Cannot proceed without config");
    });
    
    // Configure WebRTC with Google's public STUN server for NAT traversal
    let ice_servers = vec!["stun:stun.l.google.com:19302"];
    info!("[WebRTC] Configuring RTC with STUN server: {:?}", ice_servers);
    let conf = RtcConfig::new(&ice_servers);
    
    // Set up channels for signaling messages (SDP/ICE exchange)
    // These channels allow the PeerConnection instance to communicate signaling data to and from the WebSocket signaling server.
    let (tx_sig_outbound, mut rx_sig_outbound) = mpsc::channel(32);
    let (mut tx_sig_inbound, rx_sig_inbound) = mpsc::channel(32);
    
    info!("[WebRTC] Creating PeerConnection for signal_id: {}", my_id);
    let listener = PeerConnection::new(&conf, (tx_sig_outbound, rx_sig_inbound)).unwrap_or_else(|e| {
        error!("[WebRTC] Failed to create PeerConnection for {}: {:?}", my_id, e);
        panic!("PeerConnection creation failed");
    });
    info!("[WebRTC] PeerConnection created successfully for {}", my_id);
    
    let (shutdown_tx, shutdown_rx) = broadcast::channel::<()>(1);

    // Connect to signaling server via WebSocket
    let signaling_uri = config.signaling_server_address;
    let signaling_uri = format!("{}/{}", signaling_uri, my_id);
    info!("[WebRTC] Connecting to signaling server: {}", signaling_uri);

    let (mut write, mut read) = match connect_async(&signaling_uri).await {
        Ok(ws_stream) => {
            info!("[WebRTC] Successfully connected to signaling server: {}", signaling_uri);
            ws_stream.0.split()
        }
        Err(e) => {
            error!("[WebRTC] Failed to connect to signaling server {}: {:?}", signaling_uri, e);
            error!("[WebRTC] WebRTC connection setup failed for signal_id: {}", my_id);
            panic!("Signaling server connection failed: {:?}", e);
        }
    };
    let other_peer = Arc::new(Mutex::new(peer_to_dial.clone()));
    let other_peer_c = other_peer.clone();
    
    // Task: This asynchronous task listens for outgoing WebRTC signaling messages
    // produced by the PeerConnection (such as ICE candidates and SDP offers/answers).
    // For each signaling message, it wraps the message in a SignalingMessage struct,
    // specifying the intended peer's ID, serializes it to JSON, and sends it over the
    // WebSocket connection to the signaling server, which relays it to the remote peer.
    // If sending fails, an error is logged and the loop breaks.
    let mut shutdown_rx_w = shutdown_rx.resubscribe();
    let my_id_write = my_id.clone();
    let f_write = async move {
        let mut pending: VecDeque<Message> = VecDeque::new();
        loop {
            tokio::select! {
                _ = shutdown_rx_w.recv() => {
                    info!("Stopping signaling writer for {}", my_id_write);
                    break;
                }
                Some(m) = rx_sig_outbound.next() => {
                    pending.push_back(m);
                }
                else => break,
            }
            // Only send when we know the remote peer id.
            if pending.is_empty() {
                continue;
            }
            let Some(peer_id) = other_peer_c.lock().as_ref().cloned() else {
                warn!("Signaling peer id unknown; buffering {} messages", pending.len());
                continue;
            };
            while let Some(msg) = pending.pop_front() {
                let m = SignalingMessage {
                    payload: msg,
                    id: peer_id.clone(),
                };
                let s = serde_json::to_string(&m).unwrap();
                info!("[WebRTC] Sending signaling message to peer {}: {} bytes", peer_id, s.len());
                match write.send(tungstenite::Message::text(s)).await {
                    Ok(_) => {
                        info!("[WebRTC] Successfully sent signaling message to peer {}", peer_id);
                    }
                    Err(e) => {
                        error!("[WebRTC] Failed to send signaling message to peer {}: {:?}", peer_id, e);
                        error!("[WebRTC] Signaling writer task exiting due to send error");
                        break;
                    }
                }
            }
        }
        anyhow::Result::<_, anyhow::Error>::Ok(())
    };
    tokio::spawn(f_write);
    
    // Task: Receive inbound signaling messages from peer via signaling server
    // Similar to the f_write task above
    let mut shutdown_rx_r = shutdown_rx.resubscribe();
    let my_id_read = my_id.clone();
    let f_read = async move {
        loop {
            tokio::select! {
                _ = shutdown_rx_r.recv() => {
                    info!("Stopping signaling reader for {}", my_id_read);
                    break;
                }
                maybe_msg = read.next() => {
                    match maybe_msg {
                        Some(Ok(m)) => {
                            info!("[WebRTC] Received signaling message: type={:?}, size={}", 
                                m, match &m {
                                    tungstenite::Message::Text(t) => t.len(),
                                    tungstenite::Message::Binary(b) => b.len(),
                                    _ => 0,
                                });
                            if let Some(val) = match m {
                                tungstenite::Message::Text(t) => {
                                    match serde_json::from_str::<serde_json::Value>(&t) {
                                        Ok(v) => Some(v),
                                        Err(e) => {
                                            error!("[WebRTC] Failed to parse text signaling message: {}", e);
                                            continue;
                                        }
                                    }
                                }
                                tungstenite::Message::Binary(b) => {
                                    match serde_json::from_slice(&b[..]) {
                                        Ok(v) => Some(v),
                                        Err(e) => {
                                            error!("[WebRTC] Failed to parse binary signaling message: {}", e);
                                            continue;
                                        }
                                    }
                                }
                                tungstenite::Message::Close(e) => {
                                    warn!("[WebRTC] Received close message from signaling server: {:?}", e);
                                    error!("[WebRTC] Signaling server closed connection for {}", my_id_read);
                                    break;
                                }
                                _ => None,
                            } {
                                match serde_json::from_value::<SignalingMessage>(val) {
                                    Ok(c) => {
                                        info!("[WebRTC] Parsed signaling message from peer: {}", c.id);
                                        other_peer.lock().replace(c.id.clone());
                                        if tx_sig_inbound.send(c.payload).await.is_err() {
                                            error!("[WebRTC] Failed to forward signaling message to PeerConnection (channel closed)");
                                            break;
                                        }
                                        info!("[WebRTC] Successfully forwarded signaling message to PeerConnection");
                                    }
                                    Err(e) => {
                                        error!("[WebRTC] Failed to deserialize SignalingMessage: {}", e);
                                    }
                                }
                            }
                        }
                        Some(Err(e)) => {
                            error!("[WebRTC] Error reading from signaling server: {:?}", e);
                            error!("[WebRTC] Signaling reader task exiting due to read error");
                            break;
                        }
                        None => {
                            warn!("[WebRTC] Signaling server stream ended (None received)");
                            break;
                        }
                    }
                }
            }
        }
        anyhow::Result::<_, anyhow::Error>::Ok(())
    };

    tokio::spawn(f_read);

    // Establish the data channel connection
    info!("[WebRTC] Establishing data channel for signal_id: {}", my_id);
    let stream = if let Some(peer_id) = peer_to_dial {
        // We are the initiator: dial the peer
        info!("[WebRTC] Initiating connection to peer: {}", peer_id);
        match listener.dial("whatever").await {
            Ok(dc) => {
                info!("[WebRTC] Successfully dialed peer {} for signal_id {}", peer_id, my_id);
                dc
            }
            Err(e) => {
                error!("[WebRTC] Failed to dial peer {} for signal_id {}: {:?}", peer_id, my_id, e);
                error!("[WebRTC] WebRTC connection establishment failed");
                panic!("Data channel dial failed: {:?}", e);
            }
        }
    } else {
        // We are the responder: accept incoming connection
        info!("[WebRTC] Waiting to accept incoming connection for signal_id: {}", my_id);
        tokio::time::sleep(Duration::from_millis(1000)).await;
        match listener.accept().await {
            Ok(dc) => {
                info!("[WebRTC] Successfully accepted connection for signal_id: {}", my_id);
                dc
            }
            Err(e) => {
                error!("[WebRTC] Failed to accept connection for signal_id {}: {:?}", my_id, e);
                error!("[WebRTC] WebRTC connection establishment failed");
                panic!("Data channel accept failed: {:?}", e);
            }
        }
    };
    info!("[WebRTC] WebRTC data channel established successfully for signal_id: {}", my_id);
    (stream, shutdown_tx)
}

// ============================================================================
// Bidirectional GDP Packet Transfer over WebRTC
// ============================================================================

/// Manages bidirectional GDP packet transfer between WebRTC stream and ROS.
///
/// # Architecture
/// ```text
///     ROS → rtc_rx → [serialize] → WebRTC Stream
///     WebRTC Stream → [parse/reassemble] → ros_tx → ROS
/// ```
///
/// # Packet Reassembly
/// Handles fragmented packets across multiple reads using stateful buffering.
/// If a packet arrives incomplete, it's buffered until more data arrives.
#[allow(unused_assignments)]
pub async fn webrtc_reader_and_writer(
    mut stream: DataStream,
    ros_tx: UnboundedSender<GDPPacket>,       // Send parsed packets to ROS
    mut rtc_rx: UnboundedReceiver<GDPPacket>, // Receive packets from ROS to forward
) {
    info!("[WebRTC] Starting reader/writer task");
    let thread_name: GDPName = generate_random_gdp_name();
    let mut outbound_closed = false;
    let mut packets_received = 0u64;
    let mut packets_sent = 0u64;
    
    // State for reassembling fragmented packets
    let mut need_more_data_for_previous_header = false;
    let mut remaining_gdp_header: GDPHeaderInTransit = GDPHeaderInTransit {
        action: GdpAction::Noop,
        destination: GDPName([0u8, 0, 0, 0]),
        length: 0, // doesn't have any payload
    };
    let mut remaining_gdp_payload: Vec<u8> = vec![];
    let mut reset_counter = 0; // TODO: a temporary counter to reset the connection

    loop {
        let mut receiving_buf = vec![0u8; UDP_BUFFER_SIZE];
        // Wait for the UDP socket to be readable
        // or new data to be sent
        tokio::select! {
            // _ = do_stuff_async()
            // async read is cancellation safe
            // ========================================
            // RECEIVE: WebRTC → ROS
            // ========================================
            read_res = stream.read(&mut receiving_buf) => {
                let receiving_buf_size = match read_res {
                    Ok(sz) => {
                        if sz == 0 {
                            warn!("[WebRTC] Read 0 bytes - connection may be closed");
                            break;
                        }
                        sz
                    }
                    Err(e) => {
                        error!("[WebRTC] Connection error during read: {}", e);
                        error!("[WebRTC] WebRTC stream read failed - connection closed");
                        break;
                    }
                };
                let mut receiving_buf = receiving_buf[..receiving_buf_size].to_vec();
                info!("[WebRTC] Read {} bytes from data channel", receiving_buf_size);

                let mut header_payload_pair = vec!();

                // Reassemble with previous incomplete packet if needed
                if need_more_data_for_previous_header {
                    let total_payload_size = remaining_gdp_payload.len() + receiving_buf_size;
                    
                    if remaining_gdp_header.action == GdpAction::Noop {
                        // Header was incomplete/unparseable - try reparsing with more data
                        warn!("last time it had incomplete buffer to complete, the action is Noop.");
                        warn!("Incomplete header from previous read, retrying parse");
                        remaining_gdp_payload.append(&mut receiving_buf[..receiving_buf_size].to_vec());
                        receiving_buf = remaining_gdp_payload.clone();
                        reset_counter += 1;
                        
                        if reset_counter > MAX_RESET_ATTEMPTS {
                            error!("Failed to parse header after {} attempts, resetting state", MAX_RESET_ATTEMPTS);
                            receiving_buf = vec!();
                            remaining_gdp_payload = vec!();
                            reset_counter = 0;
                        }
                    }
                    else if total_payload_size < remaining_gdp_header.length { //still need more things to read!
                        // Still incomplete - buffer and wait for more data
                        info!("Need more payload data: have {}, need {}, expect {}", total_payload_size, remaining_gdp_header.length, remaining_gdp_header.length - total_payload_size);
                        remaining_gdp_payload.append(&mut receiving_buf[..receiving_buf_size].to_vec());
                        continue;
                    }
                    else if total_payload_size == remaining_gdp_header.length {
                        // Exact match - packet is now complete
                        remaining_gdp_payload.append(&mut receiving_buf[..receiving_buf_size].to_vec());
                        header_payload_pair.push((remaining_gdp_header, remaining_gdp_payload.clone()));
                        receiving_buf = vec!();
                    }
                    else { // overflow!!
                        // Overflow - buffer contains multiple packets
                        warn!("The packet is overflowed!!! read_payload_size {}, remaining_gdp_header.length {}, remaining_gdp_payload.len() {}, receiving_buf_size {}", total_payload_size, remaining_gdp_header.length, remaining_gdp_payload.len(), receiving_buf_size);

                        warn!("Buffer overflow: have {}, need {} bytes", total_payload_size, remaining_gdp_header.length);
                        let bytes_needed = remaining_gdp_header.length - remaining_gdp_payload.len();
                        remaining_gdp_payload.append(&mut receiving_buf[..bytes_needed].to_vec());
                        header_payload_pair.push((remaining_gdp_header, remaining_gdp_payload.clone()));
                        receiving_buf = receiving_buf[bytes_needed..].to_vec();
                    }
                }

                // Parse any complete packets from the buffer
                let (mut processed_gdp_packets, processed_remaining_header) = parse_header_payload_pairs(receiving_buf.to_vec());
                header_payload_pair.append(&mut processed_gdp_packets);
                
                // Forward all complete packets to ROS
                for (header, payload) in header_payload_pair {
                    let deserialized = header;

                    info!("[WebRTC] Parsed packet: action={:?}, destination={}, payload_size={}, header_length={}", 
                        deserialized.action, deserialized.destination, payload.len(), header.length);

                    if deserialized.action == GdpAction::Forward {
                        packets_received += 1;
                        info!("[WebRTC] Received Forward packet #{}: destination={}, payload_size={}", 
                            packets_received, deserialized.destination, payload.len());
                        let packet = construct_gdp_forward_from_bytes(deserialized.destination, thread_name, payload);
                        match ros_tx.send(packet) {
                            Ok(_) => {
                                info!("[WebRTC] Successfully forwarded packet #{} to ROS (destination: {})", 
                                    packets_received, deserialized.destination);
                            }
                            Err(e) => {
                                error!("[WebRTC] ROS receiver channel closed, discarding inbound packet #{}: {}", 
                                    packets_received, e);
                                error!("[WebRTC] Cannot forward packet - ROS receiver unavailable");
                                error!("[WebRTC] Total packets received before failure: {}", packets_received);
                            }
                        }
                    } else {
                        info!("[WebRTC] Received non-Forward packet (action: {:?}), not forwarding", deserialized.action);
                    }
                }

                // Update state for next read
                match processed_remaining_header {
                    Some((header, payload)) => {
                        remaining_gdp_header = header;
                        remaining_gdp_payload = payload;
                        need_more_data_for_previous_header = true;
                    },
                    None => {
                        need_more_data_for_previous_header = false;
                        remaining_gdp_payload = vec!();
                    }
                }
            },

            // ========================================
            // SEND: ROS → WebRTC
            // ========================================
            maybe_pkt_to_forward = rtc_rx.recv(), if !outbound_closed => {
                let Some(pkt_to_forward) = maybe_pkt_to_forward else {
                    info!("[WebRTC] ROS sender channel closed, stopping outbound forwarding");
                    outbound_closed = true;
                    continue;
                };
                packets_sent += 1;
                let transit_header = pkt_to_forward.get_header();
                let mut header_string = serde_json::to_string(&transit_header).unwrap();
                info!("[WebRTC] Preparing to send packet #{}: header_size={}, destination={}, action={:?}", 
                    packets_sent, header_string.len(), transit_header.destination, transit_header.action);

                //insert the first null byte to separate the packet header
                header_string.push(0u8 as char);
                let header_string_payload = header_string.as_bytes();
                match stream.write_all(&header_string_payload[..header_string_payload.len()]).await {
                    Ok(_) => {
                        info!("[WebRTC] Successfully wrote header: {} bytes", header_string_payload.len());
                    }
                    Err(e) => {
                        error!("[WebRTC] Connection error during header write: {}", e);
                        error!("[WebRTC] WebRTC stream write failed - connection closed");
                        break;
                    }
                }

                // Write payload if present
                if let Some(payload) = pkt_to_forward.payload {
                    info!("[WebRTC] Writing payload: {} bytes", payload.len());
                    match stream.write_all(&payload).await {
                        Ok(_) => {
                            info!("[WebRTC] Successfully wrote payload: {} bytes", payload.len());
                        }
                        Err(e) => {
                            error!("[WebRTC] Connection error during payload write: {}", e);
                            error!("[WebRTC] WebRTC stream write failed - connection closed");
                            break;
                        }
                    }
                }

                // Write name record if present
                if let Some(name_record) = pkt_to_forward.name_record {
                    let name_record_string = serde_json::to_string(&name_record).unwrap();
                    let name_record_bytes = name_record_string.as_bytes();
                    info!("[WebRTC] Writing name record: {} bytes", name_record_bytes.len());
                    match stream.write_all(name_record_bytes).await {
                        Ok(_) => {
                            info!("[WebRTC] Successfully wrote name record: {} bytes", name_record_bytes.len());
                        }
                        Err(e) => {
                            error!("[WebRTC] Connection error during name record write: {}", e);
                            error!("[WebRTC] WebRTC stream write failed - connection closed");
                            break;
                        }
                    }
                }
            }
        }
    }
    
    // Log final statistics when loop exits
    error!("[WebRTC] Reader/writer task exiting. Stats: packets_received={}, packets_sent={}, outbound_closed={}", 
        packets_received, packets_sent, outbound_closed);
    if packets_received == 0 && packets_sent == 0 {
        warn!("[WebRTC] No packets were processed - connection may have failed immediately");
    }
}
