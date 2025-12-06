use crate::db::{add_entity_to_database_as_transaction, get_entity_from_database};
use crate::structs::{Connection, GDPName};
use log::info;
use std::collections::{HashMap, HashSet};
use std::str::FromStr;

// Attach a subscriber to a parent (publisher or proxy) while respecting a max fan-out of 3.
// Returns true if a connection was created.
pub fn attach_subscriber(
    redis_url: &str,
    connections_topic: &str,
    publishers_topic: &str,
    proxy_topic: &str,
    topic_name: &str,
    my_gdp_name: GDPName,
) -> bool {
    let publishers = get_entity_from_database(redis_url, publishers_topic)
        .unwrap()
        .iter()
        .map(|gdp_name_string| GDPName::from_str(gdp_name_string).unwrap())
        .collect::<Vec<_>>();

    let mut connections_map = HashMap::new();
    for connection in get_entity_from_database(redis_url, connections_topic).unwrap() {
        let connection = Connection::from_str(connection.as_str()).unwrap();
        connections_map
            .entry(connection.publisher)
            .or_insert(HashSet::new())
            .insert(connection.subscriber);
    }

    let proxies = get_entity_from_database(redis_url, proxy_topic)
        .unwrap()
        .iter()
        .map(|gdpname| GDPName::from_str(gdpname).unwrap())
        .collect::<Vec<_>>();

    let mut candidates = publishers.clone();
    candidates.extend(proxies.clone());
    candidates.retain(|c| *c != my_gdp_name);

    if candidates.is_empty() {
        info!(
            "No publisher/proxy candidates available for {}; will retry later",
            my_gdp_name
        );
        return false;
    }

    let least_loaded = candidates
        .into_iter()
        .min_by_key(|c| {
            let load = connections_map
                .get(c)
                .map(|subs| subs.len())
                .unwrap_or(0);
            if load >= 3 { load + 1000 } else { load }
        })
        .unwrap();

    // If we attach to a proxy, ensure that proxy has an upstream publisher link.
    if proxies.contains(&least_loaded) && !publishers.is_empty() {
        if let Some(upstream_pub) = publishers
            .iter()
            .min_by_key(|p| {
                connections_map
                    .get(p)
                    .map(|subs| subs.len())
                    .unwrap_or(0)
            })
        {
            let already_linked = connections_map
                .get(upstream_pub)
                .map(|subs| subs.contains(&least_loaded))
                .unwrap_or(false);
            if !already_linked {
                let upstream_conn = format!("{}-{}", upstream_pub, least_loaded);
                info!(
                    "Linking publisher {} to proxy {} on topic {}",
                    upstream_pub, least_loaded, topic_name
                );
                add_entity_to_database_as_transaction(
                    redis_url,
                    connections_topic,
                    &upstream_conn,
                )
                .unwrap();
            }
        }
    } else if proxies.contains(&least_loaded) && publishers.is_empty() {
        // No publishers yet: link proxy to least-loaded other proxy if possible.
        if let Some(upstream_proxy) = proxies
            .iter()
            .filter(|p| **p != least_loaded && **p != my_gdp_name)
            .min_by_key(|p| {
                connections_map
                    .get(p)
                    .map(|subs| subs.len())
                    .unwrap_or(0)
            })
        {
            let already_linked = connections_map
                .get(upstream_proxy)
                .map(|subs| subs.contains(&least_loaded))
                .unwrap_or(false);
            if !already_linked {
                let upstream_conn = format!("{}-{}", upstream_proxy, least_loaded);
                info!(
                    "Linking proxy {} to upstream proxy {} on topic {}",
                    least_loaded, upstream_proxy, topic_name
                );
                add_entity_to_database_as_transaction(
                    redis_url,
                    connections_topic,
                    &upstream_conn,
                )
                .unwrap();
            }
        }
    }

    let connection = format!("{}-{}", least_loaded, my_gdp_name);
    let current_load = connections_map
        .get(&least_loaded)
        .map(|subs| subs.len())
        .unwrap_or(0);

    if current_load >= 3 {
        info!(
            "Parent {} is at capacity ({}); subscriber {} will retry later",
            least_loaded, current_load, my_gdp_name
        );
        return false;
    }

    info!(
        "Connecting subscriber {} to parent {} on topic {} (current load {})",
        my_gdp_name, least_loaded, topic_name, current_load
    );
    add_entity_to_database_as_transaction(redis_url, connections_topic, &connection).unwrap();
    true
}
