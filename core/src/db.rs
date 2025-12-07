use std::collections::{HashMap, HashSet};

use futures::StreamExt;
use redis::{self, transaction, Client, Commands, RedisResult};
use redis_async::client;
use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};
use utils::app_config::AppConfig;

// Get Redis URL for RIB (Routing Information Base)
pub fn get_redis_url() -> String {
    let config = AppConfig::fetch().expect("Failed to fetch config");
    format!("redis://{}", config.routing_information_base_address)
}

pub fn get_redis_address_and_port() -> Result<(String, u16), Box<dyn std::error::Error>> {
    let config = AppConfig::fetch().map_err(|e| format!("Failed to fetch config: {}", e))?;
    let url = config.routing_information_base_address;
    let mut split = url.split(":");
    let address = split
        .next()
        .ok_or_else(|| format!("Invalid Redis address format: {}", url))?
        .to_string();
    let port_str = split
        .next()
        .ok_or_else(|| format!("Missing port in Redis address: {}", url))?;
    let port = port_str
        .parse::<u16>()
        .map_err(|e| format!("Invalid port '{}' in Redis address: {}", port_str, e))?;
    Ok((address, port))
}

pub fn clear_topic_key(topic: &str) -> Result<(), Box<dyn std::error::Error>> {
    let (address, port) = get_redis_address_and_port()?;
    let client = redis::Client::open(format!("redis://{}:{}", address, port))
        .map_err(|e| format!("Failed to open Redis client: {}", e))?;
    let mut con = client
        .get_connection()
        .map_err(|e| format!("Failed to get Redis connection: {}", e))?;
    let publisher_topic = format!("{}-pub", topic);
    let subscriber_topic = format!("{}-sub", topic);

    redis::cmd("DEL")
        .arg(&publisher_topic)
        .execute(&mut con);
    redis::cmd("DEL")
        .arg(&subscriber_topic)
        .execute(&mut con);
    info!("Cleared Redis keys for topic: {} (pub: {}, sub: {})", topic, publisher_topic, subscriber_topic);
    Ok(())
}

// Atomically add publisher/subscriber to Redis list (thread-safe)
pub fn add_entity_to_database_as_transaction(
    redis_url: &str, key: &str, value: &str,
) -> RedisResult<()> {
    let client = Client::open(redis_url)?;
    let mut con = client.get_connection()?;
    let (list_length,): (isize,) = transaction(&mut con, &[key], |con, pipe| {
        pipe.lpush(key, value).query(con)
    })?;
    info!("Added entity '{}' to '{}', list length: {}", value, key, list_length);
    Ok(())
}

pub fn remove_entity_from_database_as_transaction(
    redis_url: &str, key: &str, value: &str,
) -> RedisResult<()> {
    let client = Client::open(redis_url)?;
    let mut con = client.get_connection()?;
    let (removed_count,): (isize,) = transaction(&mut con, &[key], |con, pipe| {
        pipe.lrem(key, 1, value).query(con)
    })?;
    info!("Removed {} instance(s) of '{}' from '{}'", removed_count, value, key);
    Ok(())
}

// Get all publishers/subscribers from Redis list
pub fn get_entity_from_database(redis_url: &str, key: &str) -> RedisResult<Vec<String>> {
    let client = Client::open(redis_url)?;
    let mut con = client.get_connection()?;
    let list: Vec<String> = con.lrange(key, 0, -1)?;
    Ok(list)
}

// Enable Redis keyspace notifications for dynamic discovery
// KEA = Keyspace events, Event types All
pub fn allow_keyspace_notification(redis_url: &str) -> RedisResult<()> {
    let client = Client::open(redis_url)?;
    let mut con = client.get_connection()?;
    redis::cmd("CONFIG")
        .arg("SET")
        .arg("notify-keyspace-events")
        .arg("KEA")
        .query(&mut con)
        .map_err(|e| {
            redis::RedisError::from((
                redis::ErrorKind::IoError,
                "Redis keyspace notification",
                format!("Failed to set notify-keyspace-events: {}", e),
            ))
        })?;
    info!("Enabled Redis keyspace notifications (KEA)");
    Ok(())
}

pub enum RedisListChange {
    Added(String),
    Removed(String),
}

pub async fn watch_redis_list_items(list_key: String) -> UnboundedReceiver<RedisListChange> {
    let redis_url = get_redis_url();
    if let Err(e) = allow_keyspace_notification(&redis_url) {
        error!("Failed to enable keyspace notifications: {}", e);
    }

    let (host, port) = match get_redis_address_and_port() {
        Ok(addr) => addr,
        Err(e) => {
            error!("Failed to get Redis address and port: {}", e);
            let (tx, rx) = unbounded_channel();
            drop(tx); // Close immediately to signal error
            return rx;
        }
    };

    let pubsub = match client::pubsub_connect(host.clone(), port).await {
        Ok(p) => p,
        Err(e) => {
            error!("Cannot connect to Redis pubsub at {}:{}: {}", host, port, e);
            let (tx, rx) = unbounded_channel();
            drop(tx);
            return rx;
        }
    };

    let keyspace_topic = format!("__keyspace@0__:{}", list_key);
    let mut stream = match pubsub.psubscribe(&keyspace_topic).await {
        Ok(s) => s,
        Err(e) => {
            error!("Cannot subscribe to keyspace topic '{}': {}", keyspace_topic, e);
            let (tx, rx) = unbounded_channel();
            drop(tx);
            return rx;
        }
    };

    let (tx, rx) = unbounded_channel();
    let mut known_items = HashSet::<String>::new();

    tokio::spawn(async move {
        while !tx.is_closed() {
            let items: HashSet<String> = get_entity_from_database(&redis_url, &list_key)
                .unwrap_or_default()
                .into_iter()
                .collect();

            for item in &items {
                if known_items.insert(item.clone()) {
                    let _ = tx.send(RedisListChange::Added(item.clone()));
                }
            }

            let to_remove: Vec<String> = known_items
                .iter()
                .filter(|item| !items.contains(*item))
                .cloned()
                .collect();

            for item in to_remove {
                known_items.remove(&item);
                let _ = tx.send(RedisListChange::Removed(item));
            }

            // Wait for a notification from the redis server
            loop {
                match stream.next().await {
                    Some(Ok(_)) => break,
                    Some(Err(e)) => error!("Error when waiting for redis updates: {}", e),
                    None => (),
                }
            }
        }
    });

    rx
}

/// Register a GDP name as a publisher for a topic in Redis.
pub fn register_publisher(
    redis_url: &str,
    topic_gdp: crate::structs::GDPName,
    gdp_name: crate::structs::GDPName,
    topic_name: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let publishers_key = format!("{}-publishers", topic_gdp);
    add_entity_to_database_as_transaction(redis_url, &publishers_key, &gdp_name.to_string())
        .map_err(|e| format!("Failed to register as publisher: {}", e))?;
    info!("Registered as publisher for topic: {} (GDP: {})", topic_name, topic_gdp);
    Ok(())
}

/// Get Docker container name from Docker metadata.
/// For docker-compose, this gets the unique container name like "fogros2-sgc-lite-proxy-10".
/// 
/// Docker Compose sets the container name in the format: <project>-<service>-<instance>
/// The container name is available via:
/// 1. HOSTNAME environment variable (set by docker-compose to container name)
/// 2. /etc/hostname file (contains the hostname/container name)
/// 
/// # Panics
/// Panics if container name cannot be determined - this indicates a configuration issue.
pub fn get_container_name() -> String {
    // First try CONTAINER_NAME if explicitly set (for manual overrides)
    if let Ok(name) = std::env::var("CONTAINER_NAME") {
        if !name.is_empty() {
            return name;
        }
    }
    
    // docker-compose sets HOSTNAME to the container name
    // For scaled services: "fogros2-sgc-lite-proxy-10", "fogros2-sgc-lite-listener-3", etc.
    // For non-scaled: "fogros2-sgc-lite-talker-1", "fogros2-sgc-lite-rib-1", etc.
    if let Ok(hostname) = std::env::var("HOSTNAME") {
        if !hostname.is_empty() {
            return hostname;
        }
    }
    
    // Fallback: read from /etc/hostname (should match HOSTNAME)
    if let Ok(contents) = std::fs::read_to_string("/etc/hostname") {
        let trimmed = contents.trim();
        if !trimmed.is_empty() {
            return trimmed.to_string();
        }
    }
    
    // Hard crash if container name cannot be determined
    // This indicates a serious configuration issue that needs immediate attention
    panic!(
        "CRITICAL: Cannot determine container name! \
        Checked CONTAINER_NAME env var, HOSTNAME env var, and /etc/hostname file. \
        Container name is required for GDP name mapping. \
        This is a configuration error that must be fixed."
    );
}

/// Publish GDP name -> Docker container name mapping to Redis.
/// Uses a hash map structure for efficient lookups.
pub fn publish_gdp_name_mapping(
    redis_url: &str,
    gdp_name: crate::structs::GDPName,
    container_name: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let mapping_key = format!("gdp-name-mapping:{}", gdp_name.to_string());
    add_entity_to_database_as_transaction(redis_url, &mapping_key, container_name)
        .map_err(|e| format!("Failed to publish GDP name mapping: {}", e))?;
    info!("Published GDP name mapping: {} -> {}", gdp_name.to_string(), container_name);
    Ok(())
}

/// Get container name for a specific GDP name.
pub fn get_container_name_for_gdp(redis_url: &str, gdp_name: &str) -> Result<Option<String>, Box<dyn std::error::Error>> {
    let mapping_key = format!("gdp-name-mapping:{}", gdp_name);
    let mappings = get_entity_from_database(redis_url, &mapping_key)?;
    Ok(mappings.first().cloned())
}

/// Get all GDP name -> container name mappings from Redis.
pub fn get_gdp_name_mappings(redis_url: &str) -> Result<HashMap<String, String>, Box<dyn std::error::Error>> {
    let client = Client::open(redis_url)?;
    let mut con = client.get_connection()?;
    
    // Get all keys matching the pattern
    let pattern = "gdp-name-mapping:*";
    let keys: Vec<String> = con.keys(pattern)?;
    
    let mut result = HashMap::new();
    for key in keys {
        // Extract GDP name from key (format: "gdp-name-mapping:XXXX")
        if let Some(gdp_name) = key.strip_prefix("gdp-name-mapping:") {
            let container_names: Vec<String> = con.lrange(&key, 0, -1)?;
            if let Some(container_name) = container_names.first() {
                result.insert(gdp_name.to_string(), container_name.clone());
            }
        }
    }
    Ok(result)
}

/// Register a GDP name as a proxy for a topic in Redis and connect it to the tree.
pub fn register_proxy(
    redis_url: &str,
    topic_gdp: crate::structs::GDPName,
    gdp_name: crate::structs::GDPName,
    topic_name: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let proxy_key = format!("{}-proxies", topic_gdp);
    let publishers_key = format!("{}-publishers", topic_gdp);
    let connections_key = format!("{}-connections", topic_gdp);
    
    // Register proxy in Redis
    add_entity_to_database_as_transaction(redis_url, &proxy_key, &gdp_name.to_string())
        .map_err(|e| format!("Failed to register as proxy: {}", e))?;
    info!("Registered as proxy (GDP: {})", gdp_name);
    
    // Connect proxy to tree (publisher or upstream proxy)
    // This may fail if no upstream is available yet, but proxy is still registered
    match crate::routing::connect_proxy_to_tree(
        redis_url,
        &connections_key,
        &publishers_key,
        &proxy_key,
        topic_name,
        gdp_name,
    ) {
        Ok(connected) => {
            if connected {
                info!("Proxy {} connected to tree during registration", gdp_name);
            } else {
                info!("Proxy {} registered but no upstream available yet; will connect when available", gdp_name);
            }
        }
        Err(e) => {
            error!("Failed to connect proxy {} to tree: {} (proxy still registered)", gdp_name, e);
        }
    }
    
    Ok(())
}
