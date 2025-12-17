//! FogROS2-SGC WebRTC Signaling Server
//!
//! Relays WebRTC signaling messages (SDP offers/answers, ICE candidates) between peers.
//! Also handles routing cleanup when connections disconnect.

use std::collections::{HashMap, HashSet, VecDeque};
use std::env;
use std::sync::{Arc, Mutex};

use futures_channel::mpsc;
use futures_util::{future, pin_mut, stream::TryStreamExt, StreamExt};
use serde::{Deserialize, Serialize};
use tokio::net::{TcpListener, TcpStream};
use tungstenite::handshake::server::{Request, Response};
use tungstenite::protocol::Message;

type ClientId = String;
type MessageSender = mpsc::UnboundedSender<Message>;

#[derive(Clone)]
struct Client {
    generation: u64, // Prevents stale disconnects from removing new connections
    tx: MessageSender,
}

type ClientMap = Arc<Mutex<HashMap<ClientId, Client>>>;

// --- Routing State (stored in Redis as {topic_gdp}-routing) ---

/// Must match the format used by core/routing.rs for compatibility.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct RoutingState {
    publishers: Vec<String>,
    proxies: Vec<String>,
    edges: Vec<String>,
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
    let parts: Vec<&str> = edge.split('-').collect();
    if parts.len() == 2 {
        Some((parts[0].to_string(), parts[1].to_string()))
    } else {
        None
    }
}

fn build_children_map(edges: &[String]) -> HashMap<String, Vec<String>> {
    let mut map: HashMap<String, Vec<String>> = HashMap::new();
    for edge in edges {
        if let Some((parent, child)) = parse_edge(edge) {
            map.entry(parent).or_default().push(child);
        }
    }
    for children in map.values_mut() {
        children.sort();
        children.dedup();
    }
    map
}

/// Atomically update Redis key using WATCH/MULTI/EXEC (optimistic locking).
fn atomic_update(redis_url: &str, key: &str, new_value: &str, old_value: &str) -> Result<bool, redis::RedisError> {
    let client = redis::Client::open(redis_url)?;
    let mut con = client.get_connection()?;
    
    redis::cmd("WATCH").arg(key).query::<()>(&mut con)?;
    let current: Option<String> = redis::cmd("GET").arg(key).query(&mut con)?;
    
    if current.unwrap_or_default() != old_value {
        let _ = redis::cmd("UNWATCH").query::<()>(&mut con);
        return Ok(false);
    }
    
    let mut pipe = redis::pipe();
    pipe.atomic();
    pipe.set(key, new_value);
    Ok(pipe.query::<Option<Vec<redis::Value>>>(&mut con)?.is_some())
}

/// Remove a node and its subtree from the routing state.
/// Only removes edges, not registry entries (publishers/proxies persist for rejoin).
fn disconnect_node(redis_url: &str, topic_gdp: &str, node: &str) {
    let key = format!("{}-routing", topic_gdp);
    
    for attempt in 0..32 {
        let client = match redis::Client::open(redis_url) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("[Signaling] Redis connect failed: {}", e);
                return;
            }
        };
        
        let mut con = match client.get_connection() {
            Ok(c) => c,
            Err(e) => {
                eprintln!("[Signaling] Redis connection failed: {}", e);
                return;
            }
        };
        
        let old: String = redis::cmd("GET")
            .arg(&key)
            .query(&mut con)
            .unwrap_or(None)
            .unwrap_or_default();

        let mut state: RoutingState = if old.trim().is_empty() {
            RoutingState::default()
        } else {
            serde_json::from_str(&old).unwrap_or_default()
        };
        state.normalize();

        // BFS to find all nodes in subtree rooted at `node`
        let children_map = build_children_map(&state.edges);
        let mut subtree = HashSet::new();
        let mut queue = VecDeque::new();
        subtree.insert(node.to_string());
        queue.push_back(node.to_string());
        
        while let Some(n) = queue.pop_front() {
            if let Some(children) = children_map.get(&n) {
                for child in children {
                    if subtree.insert(child.clone()) {
                        queue.push_back(child.clone());
                    }
                }
            }
        }

        // Remove edges involving subtree nodes
        state.edges.retain(|e| {
            parse_edge(e)
                .map(|(p, c)| !subtree.contains(&p) && !subtree.contains(&c))
                .unwrap_or(false)
        });
        state.normalize();

        let new_value = serde_json::to_string(&state).unwrap_or_else(|_| "{}".to_string());
        
        match atomic_update(redis_url, &key, &new_value, &old) {
            Ok(true) => {
                println!("[Signaling] Disconnected {} from topic {} (attempt {})", node, topic_gdp, attempt + 1);
                return;
            }
            Ok(false) => continue, // CAS conflict, retry
            Err(e) => {
                eprintln!("[Signaling] Redis update failed: {}", e);
                return;
            }
        }
    }
    
    eprintln!("[Signaling] Disconnect retries exceeded for {} on topic {}", node, topic_gdp);
}

// --- WebSocket Connection Handling ---

async fn handle_client(clients: ClientMap, stream: TcpStream) {
    let mut client_id = ClientId::new();

    // Extract client ID from URL path during handshake
    let callback = |req: &Request, response: Response| {
        let path = req.uri().path();
        client_id = path.split('/').nth(1).unwrap_or("unknown").to_string();
        Ok(response)
    };

    let websocket = match tokio_tungstenite::accept_hdr_async(stream, callback).await {
        Ok(ws) => ws,
        Err(e) => {
            eprintln!("[Signaling] WebSocket handshake failed: {}", e);
            return;
        }
    };
    
    println!("[Signaling] Client connected: {}", client_id);

    let (tx, rx) = mpsc::unbounded();
    
    // Register with generation counter (handles reconnects cleanly)
    let my_generation = {
        let mut locked = clients.lock().unwrap();
        let gen = locked.get(&client_id).map(|c| c.generation + 1).unwrap_or(1);
        locked.insert(client_id.clone(), Client { generation: gen, tx });
        gen
    };

    let (outgoing, incoming) = websocket.split();
    let forward = rx.map(Ok).forward(outgoing);

    // Relay messages to target peer
    let client_id_for_process = client_id.clone();
    let process = incoming.try_for_each(|msg| {
        if msg.is_text() {
            if let Ok(text) = msg.to_text() {
                if let Ok(mut content) = json::parse(text) {
                    let target_id = content["id"].to_string();
                    let locked = clients.lock().unwrap();
                    
                    if let Some(target) = locked.get(&target_id) {
                        content.insert("id", client_id_for_process.clone()).ok();
                        let _ = target.tx.unbounded_send(Message::text(json::stringify(content)));
                    }
                }
            }
        }
        future::ok(())
    });

    pin_mut!(process, forward);
    future::select(process, forward).await;

    // Cleanup on disconnect
    println!("[Signaling] Client disconnected: {}", client_id);
    
    {
        let mut locked = clients.lock().unwrap();
        if locked.get(&client_id).map(|c| c.generation == my_generation).unwrap_or(false) {
            locked.remove(&client_id);
        }
    }

    // Trigger routing cleanup after debounce
    // Client ID format: "{topic_gdp}-{self_node}-{peer_node}"
    let parts: Vec<&str> = client_id.split('-').collect();
    if parts.len() != 3 {
        return;
    }
    
    let topic_gdp = parts[0].to_string();
    let self_node = parts[1].to_string();
    let redis_url = env::var("REDIS_URL").unwrap_or_else(|_| "redis://fogros2-sgc-lite-rib-1".to_string());
    let clients_for_check = clients.clone();

    tokio::spawn(async move {
        // Debounce: allow time for reconnects before cleanup
        tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
        
        // Only clean up if no other connections exist for this node
        let still_connected = {
            let locked = clients_for_check.lock().unwrap();
            locked.keys().any(|id| {
                let p: Vec<&str> = id.split('-').collect();
                p.len() == 3 && p[0] == topic_gdp && p[1] == self_node
            })
        };
        
        if !still_connected {
            disconnect_node(&redis_url, &topic_gdp, &self_node);
        }
    });
}

#[tokio::main]
async fn main() -> Result<(), std::io::Error> {
    let port = env::args().nth(1).unwrap_or_else(|| "8000".to_string());
    let endpoint = if port.contains(':') { port } else { format!("0.0.0.0:{}", port) };

    println!("[Signaling] Starting on {}", endpoint);

    let listener = TcpListener::bind(&endpoint).await?;
    let clients: ClientMap = Arc::new(Mutex::new(HashMap::new()));

    while let Ok((stream, addr)) = listener.accept().await {
        println!("[Signaling] Connection from {}", addr);
        tokio::spawn(handle_client(clients.clone(), stream));
    }

    Ok(())
}
