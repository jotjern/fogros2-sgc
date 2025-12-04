use std::sync::Arc;

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
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tracing::{debug, error, info, warn};
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
        let header: &str = std::str::from_utf8(header_buf).unwrap();
        info!("received header json string: {:?}", header);
        
        // Try parsing JSON header
        let gdp_header_parsed = serde_json::from_str::<GDPHeaderInTransit>(header);
        if gdp_header_parsed.is_err() {
            warn!("header is not complete, return the remaining");
            return (
                header_payload_pairs,
                Some((default_gdp_header, header_buf.to_vec())),
            );
        }
        let gdp_header = gdp_header_parsed.unwrap();
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
/// An established WebRTC DataStream for bidirectional communication
pub async fn register_webrtc_stream(my_id: &str, peer_to_dial: Option<String>) -> DataStream {
    let config = AppConfig::fetch().expect("Failed to fetch config");
    
    // Configure WebRTC with Google's public STUN server for NAT traversal
    let ice_servers = vec!["stun:stun.l.google.com:19302"];
    let conf = RtcConfig::new(&ice_servers);
    
    // Set up channels for signaling messages (SDP/ICE exchange)
    // These channels allow the PeerConnection instance to communicate signaling data to and from the WebSocket signaling server.
    let (tx_sig_outbound, mut rx_sig_outbound) = mpsc::channel(32);
    let (mut tx_sig_inbound, rx_sig_inbound) = mpsc::channel(32);
    let listener = PeerConnection::new(&conf, (tx_sig_outbound, rx_sig_inbound)).unwrap();

    // Connect to signaling server via WebSocket
    let signaling_uri = config.signaling_server_address;
    let signaling_uri = format!("{}/{}", signaling_uri, my_id);
    info!("The signaling URI is {}", signaling_uri);

    let (mut write, mut read) = connect_async(&signaling_uri).await.unwrap().0.split();
    let other_peer = Arc::new(Mutex::new(peer_to_dial.clone()));
    let other_peer_c = other_peer.clone();
    
    // Task: This asynchronous task listens for outgoing WebRTC signaling messages
    // produced by the PeerConnection (such as ICE candidates and SDP offers/answers).
    // For each signaling message, it wraps the message in a SignalingMessage struct,
    // specifying the intended peer's ID, serializes it to JSON, and sends it over the
    // WebSocket connection to the signaling server, which relays it to the remote peer.
    // If sending fails, an error is logged and the loop breaks.
    let f_write = async move {
        while let Some(m) = rx_sig_outbound.next().await {
            let m = SignalingMessage {
                payload: m,
                id: other_peer_c.lock().as_ref().cloned().unwrap(),
            };
            let s = serde_json::to_string(&m).unwrap();
            info!("Sending {:?}", s);
            match write.send(tungstenite::Message::text(s)).await {
                Ok(_) => (),
                Err(e) => {
                    error!("Error sending {:?}", e);
                    break;
                }
            }
        }
        anyhow::Result::<_, anyhow::Error>::Ok(())
    };
    tokio::spawn(f_write);
    
    // Task: Receive inbound signaling messages from peer via signaling server
    // Similar to the f_write task above
    let f_read = async move {
        while let Some(Ok(m)) = read.next().await {
            info!("received {:?}", m);
            if let Some(val) = match m {
                tungstenite::Message::Text(t) => {
                    Some(serde_json::from_str::<serde_json::Value>(&t).unwrap())
                }
                tungstenite::Message::Binary(b) => Some(serde_json::from_slice(&b[..]).unwrap()),
                tungstenite::Message::Close(e) => {
                    warn!("close message {:?}", e);
                    continue;
                }
                _ => None,
            } {
                let c: SignalingMessage = serde_json::from_value(val).unwrap();
                info!("msg {:?}", c);
                other_peer.lock().replace(c.id);
                if tx_sig_inbound.send(c.payload).await.is_err() {
                    panic!()
                }
            }
        }
        anyhow::Result::<_, anyhow::Error>::Ok(())
    };

    tokio::spawn(f_read);
    
    // Establish the data channel connection
    let stream = if peer_to_dial.is_some() {
        // We are the initiator: dial the peer
        info!("dialing");
        let dc = listener.dial("whatever").await.unwrap();
        info!("dial succeed");
        dc
    } else {
        // We are the responder: accept incoming connection
        info!("accepting");
        let dc = listener.accept().await.unwrap();
        info!("accept succeed");
        dc
    };
    stream
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
    let thread_name: GDPName = generate_random_gdp_name();
    
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
                    Ok(sz) => sz,
                    Err(e) => {
                        warn!("Connection closed during read: {}", e);
                        break;
                    }
                };
                let mut receiving_buf = receiving_buf[..receiving_buf_size].to_vec();
                info!("read {} bytes", receiving_buf_size);

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
                    let deserialized = header; //TODO: change the var name here

                    info!("the total received payload with size {:} with gdp header length {}",  payload.len(), header.length);

                    if deserialized.action == GdpAction::Forward {
                        let packet = construct_gdp_forward_from_bytes(deserialized.destination, thread_name, payload);
                        ros_tx.send(packet).unwrap();
                        // proc_gdp_packet(packet,  // packet
                        //     &fib_tx,  //used to send packet to fib
                        //     &channel_tx, // used to send GDPChannel to fib
                        //     &m_tx, //the sending handle of this connection
                        //     &rib_query_tx,
                        //     "".to_string(),
                        // ).await;
                        info!("todo to be forwarded");
                    }
                    else{
                        info!("TCP received a packet but did not handle: {:?}", deserialized)
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
            maybe_pkt_to_forward = rtc_rx.recv() => {
                let Some(pkt_to_forward) = maybe_pkt_to_forward else {
                    break;
                };
                let transit_header = pkt_to_forward.get_header();
                let mut header_string = serde_json::to_string(&transit_header).unwrap();
                info!("the header size is {}", header_string.len());
                info!("the header to sent is {}", header_string);

                //insert the first null byte to separate the packet header
                header_string.push(0u8 as char);
                let header_string_payload = header_string.as_bytes();
                match stream.write_all(&header_string_payload[..header_string_payload.len()]).await {
                    Ok(_) => {},
                    Err(e) => {
                        warn!("Connection closed during write: {}", e);
                        break;
                    }
                }

                // Write payload if present
                if let Some(payload) = pkt_to_forward.payload {
                    info!("Writing payload: {} bytes", payload.len());
                    stream.write_all(&payload).await.unwrap();
                }

                // Write name record if present
                if let Some(name_record) = pkt_to_forward.name_record {
                    let name_record_string = serde_json::to_string(&name_record).unwrap();
                    let name_record_bytes = name_record_string.as_bytes();
                    info!("Writing name record: {} bytes", name_record_bytes.len());
                    stream.write_all(name_record_bytes).await.unwrap();
                }
            }
        }
    }
}
