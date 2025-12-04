extern crate futures_channel;
extern crate futures_util;
extern crate json;
extern crate redis;
/**
 * Rust signaling server example for libdatachannel
 * Copyright (c) 2020 Paul-Louis Ageneau
 *
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */
extern crate tokio;
extern crate tungstenite;

use std::collections::HashMap;
use std::env;
use std::sync::{Arc, Mutex};

use tokio::net::{TcpListener, TcpStream};
use tungstenite::handshake::server::{Request, Response};
use tungstenite::protocol::Message;

use futures_channel::mpsc;
use futures_util::stream::TryStreamExt;
use futures_util::{future, pin_mut, StreamExt};

type Id = String;
type Tx = mpsc::UnboundedSender<Message>;
type ClientsMap = Arc<Mutex<HashMap<Id, Tx>>>;

// WebRTC Signaling Server for FogROS2-SGC
//
// Acts as a message relay between WebRTC peers that need to establish direct connections.
// Listens on port 8000 for WebSocket connections at ws://server:8000/{client_id}
//
// Message Flow:
// - Client A connects → registered in HashMap with its client_id
// - Client A sends JSON: {"id": "client_b_id", "payload": {...}}
// - Server rewrites sender: {"id": "client_a_id", "payload": {...}}
// - Server forwards to Client B if connected
// - Payloads contain WebRTC signaling (SDP offers/answers, ICE candidates)
//
// Client ID Format: Each WebRTC connection establishes TWO WebSocket connections
// Publisher-side: "topic,publisher_guid,subscriber_guid"
// Subscriber-side: "topic,subscriber_guid,publisher_guid" (flipped middle/last)
//
// Example WebRTC session:
//   Publisher connects:  167,229,32,134,143,191,106,193,89,194,153,70
//   Subscriber connects: 167,229,32,134,89,194,153,70,143,191,106,193 (same bytes, flipped)
//
// Redis RIB structure:
//   {topic}-pub: stores full publisher-side client_id (topic,publisher,subscriber)
//   {topic}-sub: stores just subscriber_guid (bytes 5-8 from subscriber-side format)
//
// Redis Integration (RIB - Routing Information Base):
// - On disconnect: tries to remove from both {topic}-pub and {topic}-sub
// - Depending on which side disconnects, one removal succeeds and one warns (expected)
// - Publisher-side: successfully removes from -pub, warns on -sub
// - Subscriber-side: successfully removes from -sub, warns on -pub

/// Computes the paired client_id by swapping bytes 5-8 with bytes 9-12
/// Example: "167,229,32,134,A,A,A,A,B,B,B,B" -> "167,229,32,134,B,B,B,B,A,A,A,A"
fn get_paired_client_id(client_id: &str) -> String {
    let parts: Vec<&str> = client_id.split(',').collect();
    if parts.len() != 12 {
        return String::new();
    }
    format!(
        "{},{},{},{},{},{},{},{},{},{},{},{}",
        parts[0], parts[1], parts[2], parts[3],
        parts[8], parts[9], parts[10], parts[11],
        parts[4], parts[5], parts[6], parts[7]
    )
}

/// Removes the client (identified by `client_id`) from publisher and subscriber lists in the RIB
fn remove_client_from_rib(client_id: &str) {
    // Connect to the Redis server
    let client =
        redis::Client::open("redis://fogros2-sgc-lite-rib-1").expect("redis client open failed");
    let mut con = client.get_connection().unwrap();

    // Build topic key using the first 4 comma-separated elements of client_id
    let topic_key_name = client_id
        .split('-')
        .next()
        .unwrap();
    // Publisher and subscriber Redis list keys
    let publisher_topic = format!("{}-pub", topic_key_name.clone());
    let subscriber_topic = format!("{}-sub", topic_key_name.clone());

    println!("Removing client {} from RIB", client_id);

    // Remove client_id from the publisher list in Redis
    // WARNING: weird behavior
    // Due to an unknown reason, we need to flip the subscriber_id and
    // publisher_id to remove the client from the RIB
    let paired_client_id = get_paired_client_id(client_id);
    let result: Result<isize, redis::RedisError> =
        redis::cmd("LREM")
            .arg(&publisher_topic)
            .arg(0)
            .arg(paired_client_id)
            .query(&mut con);
    match &result {
        Ok(0) => println!(
            "Warning: client '{}' was not found in publisher list '{}'",
            client_id, publisher_topic
        ),
        Ok(count) => println!(
            "Removed client '{}' from publisher list '{}', {} occurrence(s) removed.",
            client_id, publisher_topic, count
        ),
        Err(e) => eprintln!(
            "Error removing client '{}' from publisher list '{}': {}",
            client_id, publisher_topic, e
        ),
    }

    // Extract bytes 5-8 to remove from -sub list
    // For publisher-side format (topic,pub,sub): extracts pub → removal fails (expected)
    // For subscriber-side format (topic,sub,pub): extracts sub → removal succeeds
    let subscriber_name = client_id
        .split('-')
        .nth(1)
        .unwrap();

    // Remove subscriber_name from the subscriber list in Redis
    let result: Result<isize, redis::RedisError> =
        redis::cmd("LREM")
            .arg(&subscriber_topic)
            .arg(0)
            .arg(&subscriber_name)
            .query(&mut con);
    match &result {
        Ok(0) => println!(
            "Warning: subscriber '{}' was not found in subscriber list '{}'",
            subscriber_name, subscriber_topic
        ),
        Ok(count) => println!(
            "Removed subscriber '{}' from subscriber list '{}', {} occurrence(s) removed.",
            subscriber_name, subscriber_topic, count
        ),
        Err(e) => eprintln!(
            "Error removing subscriber '{}' from subscriber list '{}': {}",
            subscriber_name, subscriber_topic, e
        ),
    }
}

/// Handle a new WebSocket client connection.
/// Registers the client, routes incoming messages, and cleans up on disconnect.
async fn handle(clients: ClientsMap, stream: TcpStream) {
    // Placeholder for client ID extracted during the handshake
    let mut client_id = Id::new();

    // Extract client ID from URL path during WebSocket handshake
    // client_id looks like "167,229,32,134,110,104,148,134,236,90,159,251"
    let callback = |req: &Request, response: Response| {
        let path: &str = req.uri().path();
        let tokens: Vec<&str> = path.split('/').collect();
        client_id = tokens[1].to_string();
        Ok(response)
    };

    // Complete websocket handshake with the client
    let websocket = tokio_tungstenite::accept_hdr_async(stream, callback)
        .await
        .expect("WebSocket handshake failed");
    println!("Client {} connected", &client_id);

    // Create an unbounded channel to allow sending messages to this client
    let (tx, rx) = mpsc::unbounded();
    clients.lock().unwrap().insert(client_id.clone(), tx);

    // Split the WebSocket for reading and writing
    let (outgoing, incoming) = websocket.split();

    // Forward outgoing messages received on the rx channel to the client WebSocket
    let forward = rx.map(Ok).forward(outgoing);

    // Process incoming messages from this client
    let process = incoming.try_for_each(|msg| {
        if msg.is_text() {
            let text = msg.to_text().unwrap();
            println!("Client {} << {}", &client_id, &text);

            // Parse the incoming JSON message
            let mut content = json::parse(text).unwrap();
            let remote_id = content["id"].to_string();
            let mut locked = clients.lock().unwrap();

            // Find the target client and forward the message
            match locked.get_mut(&remote_id) {
                Some(remote) => {
                    // Overwrite "id" to identify the true sender
                    content.insert("id", client_id.clone()).unwrap();
                    let text = json::stringify(content);

                    // Send the message to the target client
                    println!("Client {} >> {}", &remote_id, &text);
                    remote.unbounded_send(Message::text(text)).unwrap();
                }
                _ => println!("Client {} not found", &remote_id),
            }
        }
        future::ok(())
    });

    // Run both processes until one completes (client disconnect or error)
    pin_mut!(process, forward);
    future::select(process, forward).await;

    // Cleanup on client disconnect
    println!("Client {} disconnected", &client_id);
    clients.lock().unwrap().remove(&client_id);
    remove_client_from_rib(&client_id);
}

#[tokio::main]
async fn main() -> Result<(), std::io::Error> {
    // Get the listening port or address from the first command-line argument, default to "8000"
    let service = env::args().nth(1).unwrap_or("8000".to_string());

    // Construct the endpoint string - use host:port if specified, otherwise listen on all interfaces
    let endpoint = if service.contains(':') {
        service
    } else {
        format!("0.0.0.0:{}", service)
    };

    println!("Listening on {}", endpoint);

    // Bind the TCP listener to the endpoint
    let listener = TcpListener::bind(endpoint)
        .await
        .expect("Listener binding failed");

    // Shared map to keep track of connected clients
    let clients = ClientsMap::new(Mutex::new(HashMap::new()));

    // Accept incoming TCP connections in a loop
    while let Ok((stream, _)) = listener.accept().await {
        // Handle each connection in a new asynchronous task
        tokio::spawn(handle(clients.clone(), stream));
    }

    // Return Ok(()) when server shuts down (not expected to reach here under normal operation)
    Ok(())
}
