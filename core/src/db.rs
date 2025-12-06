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

pub fn get_redis_address_and_port() -> (String, u16) {
    let config = AppConfig::fetch().expect("Failed to fetch config");
    let url = config.routing_information_base_address;
    let mut split = url.split(":");
    let address = split.next().unwrap().to_string();
    let port = split.next().unwrap().parse::<u16>().unwrap();
    (address, port)
}

pub fn clear_topic_key(topic: &str) {
    let (address, port) = get_redis_address_and_port();
    let client = redis::Client::open(format!("redis://{}:{}", address, port)).unwrap();
    let mut con = client.get_connection().unwrap();
    let publisher_topic = format!("{}-pub", topic);
    let subscriber_topic = format!("{}-sub", topic);

    redis::cmd("DEL").arg(publisher_topic).execute(&mut con);
    redis::cmd("DEL").arg(subscriber_topic).execute(&mut con);
}

// Atomically add publisher/subscriber to Redis list (thread-safe)
pub fn add_entity_to_database_as_transaction(
    redis_url: &str, key: &str, value: &str,
) -> RedisResult<()> {
    let client = Client::open(redis_url)?;
    let mut con = client.get_connection()?;
    let (new_val,): (isize,) = transaction(&mut con, &[key], |con, pipe| {
        pipe.lpush(key, value).query(con)
    })?;
    println!("The incremented number is: {}", new_val);
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
    let _: () = redis::cmd("CONFIG")
        .arg("SET")
        .arg("notify-keyspace-events")
        .arg("KEA")
        .query(&mut con)
        .expect("failed to execute SET for notify-keyspace-events");

    Ok(())
}

pub enum RedisListChange {
    Added(String),
    Removed(String),
}

pub async fn watch_redis_list_items(list_key: String) -> UnboundedReceiver<RedisListChange> {
    let redis_url = get_redis_url();
    allow_keyspace_notification(&redis_url).unwrap();

    let (host, port) = get_redis_address_and_port();
    let pubsub = client::pubsub_connect(host, port)
        .await
        .expect("Cannot connect to Redis pubsub");

    let keyspace_topic = format!("__keyspace@0__:{}", list_key);
    let mut stream = pubsub
        .psubscribe(&keyspace_topic)
        .await
        .expect("Cannot subscribe");

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
