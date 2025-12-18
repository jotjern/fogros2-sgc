//! Redis/RIB (Routing Information Base) helpers.
//!
//! Provides optimistic CAS updates (WATCH/MULTI/EXEC) for routing state,
//! keyspace notifications for dynamic discovery, and GDP name mapping.

use futures::StreamExt;
use log::{error, info};
use redis::{self, Client, Commands, RedisResult};
use redis_async::client;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver};
use utils::app_config::AppConfig;
use crate::structs::GDPName;

/// Get Redis URL from config (e.g., "redis://host:6379").
pub fn get_redis_url() -> String {
    let config = AppConfig::fetch().expect("Failed to fetch config");
    format!("redis://{}", config.routing_server)
}

/// Parse routing_server into (host, port).
pub fn get_redis_address_and_port() -> Result<(String, u16), Box<dyn std::error::Error>> {
    let config = AppConfig::fetch().map_err(|e| format!("Failed to fetch config: {}", e))?;
    let url = config.routing_server;
    let mut split = url.split(':');
    let address = split
        .next()
        .ok_or_else(|| format!("Invalid routing_server format: {}", url))?
        .to_string();
    let port_str = split
        .next()
        .ok_or_else(|| format!("Missing port in routing_server: {}", url))?;
    let port = port_str
        .parse::<u16>()
        .map_err(|e| format!("Invalid port '{}': {}", port_str, e))?;
    Ok((address, port))
}

/// Clear all Redis keys for a topic (for testing/reset).
pub fn clear_topic_key(topic: &str) -> Result<(), Box<dyn std::error::Error>> {
    let (address, port) = get_redis_address_and_port()?;
    let client = redis::Client::open(format!("redis://{}:{}", address, port))
        .map_err(|e| format!("Failed to open Redis client: {}", e))?;
    let mut con = client
        .get_connection()
        .map_err(|e| format!("Failed to get Redis connection: {}", e))?;
    
    let keys = [
        format!("{}-routing", topic),
        format!("{}-publishers", topic),
        format!("{}-proxies", topic),
        format!("{}-connections", topic),
        format!("{}-pub", topic),
        format!("{}-sub", topic),
    ];
    for k in &keys {
        redis::cmd("DEL").arg(k).query::<()>(&mut con)?;
    }
    info!("Cleared Redis topic keys for: {}", topic);
    Ok(())
}

/// Try a single optimistic compare-and-swap update.
/// Returns Ok(true) if committed, Ok(false) if concurrent modification.
pub fn try_atomic_update(
    redis_url: &str,
    key: &str,
    new_value: &str,
    old_value: &str,
) -> RedisResult<bool> {
    let client = Client::open(redis_url)?;
    let mut con = client.get_connection()?;

    redis::cmd("WATCH").arg(key).query::<()>(&mut con)?;
    let current: Option<String> = con.get(key)?;
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

/// Retry optimistic CAS up to max_retries times.
pub fn atomic_update(
    redis_url: &str,
    key: &str,
    new_value: &str,
    old_value: &str,
    max_retries: usize,
) -> RedisResult<bool> {
    for _ in 0..max_retries {
        if try_atomic_update(redis_url, key, new_value, old_value)? {
            return Ok(true);
        }
    }
    Ok(false)
}

pub fn get_string(redis_url: &str, key: &str) -> RedisResult<Option<String>> {
    let client = Client::open(redis_url)?;
    let mut con = client.get_connection()?;
    con.get(key)
}

/// Enable Redis keyspace notifications (KEA = all events).
pub fn allow_keyspace_notification(redis_url: &str) -> RedisResult<()> {
    let client = Client::open(redis_url)?;
    let mut con = client.get_connection()?;
    redis::cmd("CONFIG")
        .arg("SET")
        .arg("notify-keyspace-events")
        .arg("KEA")
        .query::<()>(&mut con)
        .map_err(|e| {
            redis::RedisError::from((
                redis::ErrorKind::IoError,
                "Redis keyspace notification",
                format!("Failed to set notify-keyspace-events: {}", e),
            ))
        })?;
    info!("Enabled Redis keyspace notifications");
    Ok(())
}

#[derive(Debug, Clone)]
pub struct RedisKeyChange {
    pub key: String,
    pub event: String,
}

/// Watch a Redis key via keyspace notifications.
pub async fn watch_redis_key(key: String) -> UnboundedReceiver<RedisKeyChange> {
    let redis_url = get_redis_url();
    if let Err(e) = allow_keyspace_notification(&redis_url) {
        error!("Failed to enable keyspace notifications: {}", e);
    }

    let (host, port) = match get_redis_address_and_port() {
        Ok(addr) => addr,
        Err(e) => {
            error!("Failed to parse routing_server: {}", e);
            let (tx, rx) = unbounded_channel();
            drop(tx);
            return rx;
        }
    };

    let pubsub = match client::pubsub_connect(host.clone(), port).await {
        Ok(p) => p,
        Err(e) => {
            error!("Cannot connect to Redis at {}:{}: {}", host, port, e);
            let (tx, rx) = unbounded_channel();
            drop(tx);
            return rx;
        }
    };

    let keyspace_topic = format!("__keyspace@0__:{}", key);
    let mut stream = match pubsub.psubscribe(&keyspace_topic).await {
        Ok(s) => s,
        Err(e) => {
            error!("Cannot subscribe to '{}': {}", keyspace_topic, e);
            let (tx, rx) = unbounded_channel();
            drop(tx);
            return rx;
        }
    };

    let (tx, rx) = unbounded_channel();

    tokio::spawn(async move {
        while !tx.is_closed() {
            loop {
                match stream.next().await {
                    Some(Ok(_msg)) => {
                        let _ = tx.send(RedisKeyChange { 
                            key: key.clone(), 
                            event: "changed".to_string() 
                        });
                        break;
                    }
                    Some(Err(e)) => error!("Redis watch error: {}", e),
                    None => (),
                }
            }
        }
    });

    rx
}

/// Get hostname (HOSTNAME env var or "unknown").
pub fn get_container_name() -> String {
    std::env::var("HOSTNAME").unwrap_or_else(|_| "unknown".to_string())
}

/// Store GDP name -> hostname mapping in Redis (for dashboard).
pub fn publish_gdp_name_mapping(
    redis_url: &str,
    gdp_name: GDPName,
    container_name: &str,
) -> RedisResult<()> {
    let client = Client::open(redis_url)?;
    let mut con = client.get_connection()?;
    let _: () = con.hset("gdpname_map", gdp_name.to_string(), container_name)?;
    Ok(())
}

/// Test Redis connectivity. Returns Ok(()) if PING succeeds.
pub fn test_redis_connection(redis_url: &str) -> RedisResult<()> {
    let client = Client::open(redis_url)?;
    let mut con = client.get_connection()?;
    redis::cmd("PING").query::<String>(&mut con)?;
    Ok(())
}
