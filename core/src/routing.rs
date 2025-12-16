//! Routing system (rigorous model, no fixups).
//!
//! State is stored as a single Redis value per topic: `{topic_gdp}-routing` (JSON).
//! All subscribe/disconnect operations are optimistic CAS updates using WATCH/MULTI/EXEC.
//!
//! Invariants (maintained by construction):
//! - `publishers` is unique and preserves insertion order (publisher[0] is the root).
//! - `proxies` is unique.
//! - `edges` contains unique (parent, child) pairs.
//! - For all parents, `children.len() <= FANOUT`.

use crate::db::{atomic_update, get_string};
use crate::structs::{Connection, GDPName};
use log::{debug, info, warn};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet, VecDeque};
use std::str::FromStr;

const FANOUT: usize = 3;
const MAX_RETRIES: usize = 32;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
struct Edge {
    parent: String,
    child: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct RoutingState {
    /// Hex GDPName strings. Unique, insertion-ordered.
    publishers: Vec<String>,
    /// Hex GDPName strings. Unique.
    proxies: Vec<String>,
    /// Unique edges.
    edges: Vec<Edge>,
}

impl RoutingState {
    fn has_publisher(&self, n: &str) -> bool {
        self.publishers.iter().any(|p| p == n)
    }

    fn has_proxy(&self, n: &str) -> bool {
        self.proxies.iter().any(|p| p == n)
    }

    fn add_publisher(&mut self, n: String) {
        if !self.has_publisher(&n) {
            self.publishers.push(n);
        }
    }

    fn add_proxy(&mut self, n: String) {
        if !self.has_proxy(&n) {
            self.proxies.push(n);
        }
    }

    fn is_proxy(&self, n: &str) -> bool {
        self.has_proxy(n)
    }

    fn is_intermediate(&self, n: &str) -> bool {
        self.has_publisher(n) || self.has_proxy(n)
    }

    fn has_edge(&self, parent: &str, child: &str) -> bool {
        self.edges.iter().any(|e| e.parent == parent && e.child == child)
    }

    fn add_edge_unique(&mut self, parent: String, child: String) {
        if !self.has_edge(&parent, &child) {
            self.edges.push(Edge { parent, child });
        }
    }

    fn remove_edge(&mut self, parent: &str, child: &str) {
        self.edges.retain(|e| !(e.parent == parent && e.child == child));
    }

    fn has_parent(&self, node: &str) -> bool {
        self.edges.iter().any(|e| e.child == node)
    }
}

fn routing_key(topic_gdp: GDPName) -> String {
    format!("{}-routing", topic_gdp)
}

fn parse_state(s: &str) -> RoutingState {
    if s.trim().is_empty() {
        return RoutingState::default();
    }
    serde_json::from_str(s).unwrap_or_default()
}

fn to_json(state: &RoutingState) -> String {
    serde_json::to_string(state).unwrap_or_else(|_| "{}".to_string())
}

fn build_children_map(edges: &[Edge]) -> HashMap<String, Vec<String>> {
    let mut out: HashMap<String, Vec<String>> = HashMap::new();
    for e in edges {
        out.entry(e.parent.clone()).or_default().push(e.child.clone());
    }
    // Deterministic order.
    for v in out.values_mut() {
        v.sort();
        v.dedup();
    }
    out
}

fn bfs_intermediates(state: &RoutingState, root: &str, children: &HashMap<String, Vec<String>>) -> Vec<String> {
    let mut out = Vec::new();
    let mut q = VecDeque::new();
    let mut seen = HashSet::<String>::new();

    q.push_back(root.to_string());
    seen.insert(root.to_string());

    while let Some(n) = q.pop_front() {
        out.push(n.clone());
        let Some(ch) = children.get(&n) else { continue };
        for c in ch {
            if state.is_intermediate(c) {
                if seen.insert(c.clone()) {
                    q.push_back(c.clone());
                }
            }
        }
    }

    out
}

fn used_nodes_set(state: &RoutingState) -> HashSet<String> {
    let mut used = HashSet::<String>::new();
    for e in &state.edges {
        used.insert(e.parent.clone());
        used.insert(e.child.clone());
    }
    for p in &state.publishers {
        used.insert(p.clone());
    }
    used
}

fn pick_unused_proxy(state: &RoutingState) -> Option<String> {
    let used = used_nodes_set(state);
    state.proxies.iter().find(|p| !used.contains(*p)).cloned()
}

/// Register this node as a publisher for the topic.
pub fn register_publisher(redis_url: &str, topic_gdp: GDPName, publisher: GDPName, topic_name: &str) -> Result<(), String> {
    let key = routing_key(topic_gdp);
    for attempt in 0..MAX_RETRIES {
        let old = get_string(redis_url, &key).unwrap_or(None).unwrap_or_default();
        let mut st = parse_state(&old);
        let before = st.publishers.len();
        st.add_publisher(publisher.to_string());
        let after = st.publishers.len();

        let new = to_json(&st);
        if atomic_update(redis_url, &key, &new, &old, 1).map_err(|e| e.to_string())? {
            info!("Registered publisher {} for topic {}", publisher, topic_name);
            debug!(
                "[routing] register_publisher attempt={} topic={} key={} publisher={} publishers_len {}->{} proxies_len={} edges_len={}",
                attempt,
                topic_gdp,
                key,
                publisher,
                before,
                after,
                st.proxies.len(),
                st.edges.len()
            );
            return Ok(());
        }
    }
    Err(format!(
        "Failed to register publisher {} for topic {} (CAS retries exceeded)",
        publisher, topic_name
    ))
}

/// Register this node as a proxy candidate for the topic.
pub fn register_proxy(redis_url: &str, topic_gdp: GDPName, proxy: GDPName, topic_name: &str) -> Result<(), String> {
    let key = routing_key(topic_gdp);
    for attempt in 0..MAX_RETRIES {
        let old = get_string(redis_url, &key).unwrap_or(None).unwrap_or_default();
        let mut st = parse_state(&old);
        let before = st.proxies.len();
        st.add_proxy(proxy.to_string());
        let after = st.proxies.len();

        let new = to_json(&st);
        if atomic_update(redis_url, &key, &new, &old, 1).map_err(|e| e.to_string())? {
            info!("Registered proxy {} for topic {}", proxy, topic_name);
            debug!(
                "[routing] register_proxy attempt={} topic={} key={} proxy={} proxies_len {}->{} publishers_len={} edges_len={}",
                attempt,
                topic_gdp,
                key,
                proxy,
                before,
                after,
                st.publishers.len(),
                st.edges.len()
            );
            return Ok(());
        }
    }
    Err(format!(
        "Failed to register proxy {} for topic {} (CAS retries exceeded)",
        proxy, topic_name
    ))
}

/// Subscribe (join) this node as a listener to the topic.
///
/// This performs one CAS update of `{topic}-routing` (retried on conflicts).
pub fn subscribe(redis_url: &str, topic_gdp: GDPName, topic_name: &str, subscriber: GDPName) -> Result<(), String> {
    let key = routing_key(topic_gdp);
    let subscriber_s = subscriber.to_string();

    for attempt in 0..MAX_RETRIES {
        let old = get_string(redis_url, &key).unwrap_or(None).unwrap_or_default();
        let mut st = parse_state(&old);

        debug!(
            "[routing] subscribe attempt={} topic={} key={} subscriber={} publishers_len={} proxies_len={} edges_len={}",
            attempt,
            topic_gdp,
            key,
            subscriber,
            st.publishers.len(),
            st.proxies.len(),
            st.edges.len()
        );

        if st.publishers.is_empty() {
            return Err(format!("No publishers registered yet for topic {}", topic_name));
        }
        let root = st.publishers[0].clone();
        debug!(
            "[routing] subscribe topic={} subscriber={} root={} publishers={:?}",
            topic_gdp, subscriber, root, st.publishers
        );

        // Idempotent: already has a parent edge.
        if st.has_parent(&subscriber_s) {
            debug!(
                "[routing] subscribe topic={} subscriber={} already_attached=true",
                topic_gdp, subscriber
            );
            return Ok(());
        }

        let children = build_children_map(&st.edges);
        let nodes = bfs_intermediates(&st, &root, &children);
        debug!(
            "[routing] subscribe topic={} subscriber={} bfs_nodes={:?}",
            topic_gdp, subscriber, nodes
        );

        // Pass 1: attach to first BFS node with spare fanout.
        for n in &nodes {
            let cnt = children.get(n).map(|v| v.len()).unwrap_or(0);
            debug!(
                "[routing] subscribe topic={} subscriber={} pass=1 node={} children={} fanout={}",
                topic_gdp, subscriber, n, cnt, FANOUT
            );
            if cnt < FANOUT {
                info!(
                    "[routing] subscribe decision topic={} subscriber={} action=attach parent={} reason=capacity children={}/{}",
                    topic_gdp, subscriber, n, cnt, FANOUT
                );
                st.add_edge_unique(n.clone(), subscriber_s.clone());

                let new = to_json(&st);
                if atomic_update(redis_url, &key, &new, &old, 1).map_err(|e| e.to_string())? {
                    info!("Subscribed {} to topic {} under {}", subscriber, topic_name, n);
                    return Ok(());
                }
                debug!(
                    "[routing] subscribe topic={} subscriber={} action=attach parent={} result=cas_conflict retrying",
                    topic_gdp, subscriber, n
                );
                continue;
            }
        }

        // Pass 2: no capacity anywhere => grow with a proxy graft.
        // We must keep fanout consistent for both the parent and the new proxy.
        for n in &nodes {
            let ch = children.get(n).cloned().unwrap_or_default();
            if ch.is_empty() {
                debug!(
                    "[routing] subscribe topic={} subscriber={} pass=2 node={} skip=empty_children",
                    topic_gdp, subscriber, n
                );
                continue;
            }
            if ch.iter().all(|c| st.is_proxy(c)) {
                debug!(
                    "[routing] subscribe topic={} subscriber={} pass=2 node={} skip=all_children_are_proxies children={:?}",
                    topic_gdp, subscriber, n, ch
                );
                continue;
            }

            let Some(new_proxy) = pick_unused_proxy(&st) else {
                return Err(format!("No unused proxies available to grow routing tree for topic {}", topic_name));
            };

            // Parent `n` is full (otherwise pass-1 would have attached). After adding `new_proxy`,
            // we must move >= 1 child off `n`.
            // `new_proxy` must have <= FANOUT children, and it must include `subscriber`,
            // so we may move at most FANOUT-1 existing children.
            let mut non_proxy_children: Vec<String> = ch.iter().filter(|c| !st.is_proxy(c)).cloned().collect();
            // deterministic: `ch` is sorted already
            let move_count = non_proxy_children.len().min(FANOUT - 1).max(1);
            non_proxy_children.truncate(move_count);

            info!(
                "[routing] subscribe decision topic={} subscriber={} action=graft_proxy parent={} new_proxy={} reason=no_capacity move_children_count={} children={:?}",
                topic_gdp, subscriber, n, new_proxy, move_count, ch
            );

            // Add proxy under parent.
            st.add_edge_unique(n.clone(), new_proxy.clone());
            // Attach subscriber under proxy.
            st.add_edge_unique(new_proxy.clone(), subscriber_s.clone());
            // Move bounded set of non-proxy children under proxy.
            for c in non_proxy_children {
                st.remove_edge(n, &c);
                st.add_edge_unique(new_proxy.clone(), c);
            }

            let new = to_json(&st);
            if atomic_update(redis_url, &key, &new, &old, 1).map_err(|e| e.to_string())? {
                info!("Subscribed {} to topic {} via new proxy {}", subscriber, topic_name, new_proxy);
                return Ok(());
            }
            debug!(
                "[routing] subscribe topic={} subscriber={} action=graft_proxy parent={} new_proxy={} result=cas_conflict retrying",
                topic_gdp, subscriber, n, new_proxy
            );
        }

        warn!("Subscribe({}) could not find a place; retrying", subscriber);
    }

    Err(format!(
        "Failed to subscribe {} to topic {} (CAS retries exceeded)",
        subscriber, topic_name
    ))
}

/// Disconnect a node: detach the entire subtree rooted at `node` from the routing tree.
///
/// Per plan: disconnect only detaches edges; it does not remove nodes from registries.
pub fn disconnect(redis_url: &str, topic_gdp: GDPName, node: GDPName) -> Result<(), String> {
    let key = routing_key(topic_gdp);
    let node_s = node.to_string();

    for attempt in 0..MAX_RETRIES {
        let old = get_string(redis_url, &key).unwrap_or(None).unwrap_or_default();
        let mut st = parse_state(&old);

        debug!(
            "[routing] disconnect attempt={} topic={} key={} node={} publishers_len={} proxies_len={} edges_len={}",
            attempt,
            topic_gdp,
            key,
            node,
            st.publishers.len(),
            st.proxies.len(),
            st.edges.len()
        );

        let children = build_children_map(&st.edges);

        // BFS subtree.
        let mut subtree = HashSet::<String>::new();
        let mut q = VecDeque::<String>::new();
        subtree.insert(node_s.clone());
        q.push_back(node_s.clone());
        while let Some(n) = q.pop_front() {
            if let Some(ch) = children.get(&n) {
                for c in ch {
                    if subtree.insert(c.clone()) {
                        q.push_back(c.clone());
                    }
                }
            }
        }

        let before_edges = st.edges.len();
        st.edges.retain(|e| !subtree.contains(&e.parent) && !subtree.contains(&e.child));
        let after_edges = st.edges.len();

        let new = to_json(&st);
        if atomic_update(redis_url, &key, &new, &old, 1).map_err(|e| e.to_string())? {
            info!("Disconnected {} from topic {}", node, topic_gdp);
            info!(
                "[routing] disconnect committed topic={} node={} edges_removed={}",
                topic_gdp,
                node,
                before_edges.saturating_sub(after_edges)
            );
            return Ok(());
        }

        debug!(
            "[routing] disconnect topic={} node={} result=cas_conflict retrying",
            topic_gdp, node
        );
    }

    Err(format!(
        "Failed to disconnect {} from topic {} (CAS retries exceeded)",
        node, topic_gdp
    ))
}

/// Read the current routing edges as `Connection`s.
pub fn current_connections(redis_url: &str, topic_gdp: GDPName) -> Result<Vec<Connection>, String> {
    let key = routing_key(topic_gdp);
    let raw = get_string(redis_url, &key).unwrap_or(None).unwrap_or_default();
    let st = parse_state(&raw);

    let mut out = Vec::new();
    for e in st.edges {
        let s = format!("{}-{}", e.parent, e.child);
        if let Ok(c) = Connection::from_str(&s) {
            out.push(c);
        }
    }
    Ok(out)
}
