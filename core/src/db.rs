use std::collections::HashSet;

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
