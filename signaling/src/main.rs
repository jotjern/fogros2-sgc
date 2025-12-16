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
#[derive(Clone)]
struct ClientEntry {
    gen: u64,
    tx: Tx,
}
type ClientsMap = Arc<Mutex<HashMap<Id, ClientEntry>>>;

// WebRTC Signaling Server
//
// On websocket disconnect, we trigger routing cleanup:
// - Parse client_id as `{topic_gdp}-{self}-{peer}` (hex GDPName strings)
// - Call `disconnect(self)` on `{topic_gdp}-routing` in Redis (WATCH/MULTI/EXEC CAS)

use serde::{Deserialize, Serialize};
use std::collections::{HashMap as StdHashMap, HashSet, VecDeque};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct RoutingState {
    publishers: Vec<String>,
    proxies: Vec<String>,
    edges: Vec<String>, // "parent-child"
}

impl RoutingState {
    fn normalize(&mut self) {
        self.publishers.sort();
        self.publishers.dedup();
        self.proxies.sort();
        self.proxies.dedup();
        self.edges.sort();
        self.edges.dedup();
    }
}

fn parse_edge(edge: &str) -> Option<(String, String)> {
    let mut parts = edge.split('-');
    let a = parts.next()?.to_string();
    let b = parts.next()?.to_string();
    if parts.next().is_some() {
        return None;
    }
    Some((a, b))
}

fn build_children_map(edges: &[String]) -> StdHashMap<String, Vec<String>> {
    let mut out: StdHashMap<String, Vec<String>> = StdHashMap::new();
    for e in edges {
        if let Some((p, c)) = parse_edge(e) {
            out.entry(p).or_default().push(c);
        }
    }
    for v in out.values_mut() {
        v.sort();
        v.dedup();
    }
    out
}

fn try_atomic_update(redis_url: &str, key: &str, new_value: &str, old_value: &str) -> Result<bool, redis::RedisError> {
    let client = redis::Client::open(redis_url)?;
    let mut con = client.get_connection()?;
    redis::cmd("WATCH").arg(key).query::<()>(&mut con)?;
    let current: Option<String> = redis::cmd("GET").arg(key).query(&mut con)?;
    let current = current.unwrap_or_default();
    if current != old_value {
        let _ = redis::cmd("UNWATCH").query::<()>(&mut con);
        return Ok(false);
    }
    let mut pipe = redis::pipe();
    pipe.atomic();
    pipe.set(key, new_value);
    let exec_result = pipe.query::<Option<Vec<redis::Value>>>(&mut con)?;
    Ok(exec_result.is_some())
}

fn disconnect_from_routing(redis_url: &str, topic_gdp: &str, node: &str) {
    let key = format!("{}-routing", topic_gdp);
    for _ in 0..32 {
        let client = match redis::Client::open(redis_url) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("Redis open failed: {}", e);
                return;
            }
        };
        let mut con = match client.get_connection() {
            Ok(c) => c,
            Err(e) => {
                eprintln!("Redis connection failed: {}", e);
                return;
            }
        };
        let old: Option<String> = match redis::cmd("GET").arg(&key).query(&mut con) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("Redis GET {} failed: {}", key, e);
                return;
            }
        };
        let old = old.unwrap_or_default();

        let mut st: RoutingState = if old.trim().is_empty() {
            RoutingState::default()
        } else {
            serde_json::from_str(&old).unwrap_or_default()
        };
        st.normalize();

        let children = build_children_map(&st.edges);
        let mut subtree = HashSet::<String>::new();
        let mut q = VecDeque::<String>::new();
        subtree.insert(node.to_string());
        q.push_back(node.to_string());
        while let Some(n) = q.pop_front() {
            if let Some(ch) = children.get(&n) {
                for c in ch {
                    if subtree.insert(c.clone()) {
                        q.push_back(c.clone());
                    }
                }
            }
        }

        st.edges.retain(|e| {
            let Some((p, c)) = parse_edge(e) else { return false };
            !subtree.contains(&p) && !subtree.contains(&c)
        });
        // IMPORTANT (per plan): disconnect only detaches edges.
        // Do NOT delete nodes from `publishers`/`proxies` registries here.
        st.normalize();

        let new_value = serde_json::to_string(&st).unwrap_or_else(|_| "{}".to_string());
        match try_atomic_update(redis_url, &key, &new_value, &old) {
            Ok(true) => {
                println!("Disconnected node {} from topic {} (routing cleanup)", node, topic_gdp);
                return;
            }
            Ok(false) => continue, // retry on conflict
            Err(e) => {
                eprintln!("Redis CAS update failed: {}", e);
                return;
            }
        }
    }
    eprintln!("Routing cleanup retries exceeded for topic {} node {}", topic_gdp, node);
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
    // IMPORTANT: a client_id may reconnect while an old websocket is still winding down.
    // If we overwrite the map entry, the old websocket's cleanup would remove the *new* entry,
    // causing flapping. Use a generation counter so only the latest connection can remove itself.
    let my_gen = {
        let mut locked = clients.lock().unwrap();
        let gen = locked.get(&client_id).map(|e| e.gen + 1).unwrap_or(1);
        locked.insert(client_id.clone(), ClientEntry { gen, tx: tx.clone() });
        gen
    };

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
            let locked = clients.lock().unwrap();

            // Find the target client and forward the message
            match locked.get(&remote_id) {
                Some(remote) => {
                    // Overwrite "id" to identify the true sender
                    content.insert("id", client_id.clone()).unwrap();
                    let text = json::stringify(content);

                    // Send the message to the target client
                    println!("Client {} >> {}", &remote_id, &text);
                    remote.tx.unbounded_send(Message::text(text)).unwrap();
                }
                None => eprintln!("ERROR: Client {} not found", &remote_id),
            }
        }
        future::ok(())
    });

    // Run both processes until one completes (client disconnect or error)
    pin_mut!(process, forward);
    future::select(process, forward).await;

    // Cleanup on client disconnect
    println!("Client {} disconnected", &client_id);
    {
        let mut locked = clients.lock().unwrap();
        // Only remove if we are still the latest generation for this client_id.
        let should_remove = locked.get(&client_id).map(|e| e.gen == my_gen).unwrap_or(false);
        if should_remove {
            locked.remove(&client_id);
        } else {
            println!(
                "Ignoring disconnect for stale generation: id={} gen={} (latest differs)",
                client_id, my_gen
            );
        }
    }

    // IMPORTANT: the signaling server has ONE websocket per *edge endpoint*.
    // A single websocket disconnect does NOT imply the node is gone; it may still have other edges.
    //
    // To avoid routing flapping, only call disconnect(self_node) when this was the *last*
    // websocket for that (topic_gdp, self_node), and after a short debounce to allow reconnects.
    let parts: Vec<&str> = client_id.split('-').collect();
    if parts.len() != 3 {
        eprintln!("Unexpected client_id format (expected topic-self-peer): {}", client_id);
        return;
    }
    let topic_gdp = parts[0].to_string();
    let self_node = parts[1].to_string();
    let redis_url = env::var("REDIS_URL").unwrap_or_else(|_| "redis://fogros2-sgc-lite-rib-1".to_string());
    let clients_for_check = clients.clone();

    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
        let still_has_any = {
            let locked = clients_for_check.lock().unwrap();
            locked.keys().any(|id| {
                let p: Vec<&str> = id.split('-').collect();
                p.len() == 3 && p[0] == topic_gdp && p[1] == self_node
            })
        };
        if still_has_any {
            println!(
                "Skip routing disconnect for topic {} node {} (other websockets still connected)",
                topic_gdp, self_node
            );
            return;
        }
        println!(
            "Routing disconnect for topic {} node {} (last websocket disconnected)",
            topic_gdp, self_node
        );
        disconnect_from_routing(&redis_url, &topic_gdp, &self_node);
    });
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
