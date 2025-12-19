//! Routing system (rigorous model, no fixups).
//!
//! State is stored as a single Redis value per topic: `{topic_gdp}-routing` (JSON).
//! All subscribe/disconnect operations are optimistic CAS updates using WATCH/MULTI/EXEC.
//!
//! Invariants (maintained by construction):
//! - `publishers` is unique and preserves insertion order (publisher[0] is the root).
//! - `proxies` is unique.
//! - `edges` contains unique (parent, child) pairs.
//! - For all parents, `children.len() <= fanout()`.

use crate::db::{atomic_update, get_container_name, get_string, is_proxy_alive};
use crate::structs::{Connection, GDPName};
use log::{debug, info, warn};
use redis::Commands;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet, VecDeque};
use std::str::FromStr;
use std::sync::OnceLock;
use std::time::Duration;
use std::time::{SystemTime, UNIX_EPOCH};

const DEFAULT_FANOUT: usize = 3;
// During large concurrent joins, optimistic CAS can conflict frequently. If we bail out too
// early, the caller may end up retrying at a much coarser timescale (seconds), which inflates
// join-latency tails. Use a higher retry budget with lightweight backoff.
const MAX_RETRIES: usize = 128;
static FANOUT: OnceLock<usize> = OnceLock::new();

fn fanout() -> usize {
    *FANOUT.get_or_init(|| {
        std::env::var("FANOUT_FACTOR")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .filter(|n| (1..=1024).contains(n))
            .unwrap_or(DEFAULT_FANOUT)
    })
}

fn routing_trace() -> bool {
    std::env::var("SGC_ROUTING_TRACE")
        .ok()
        .map(|v| matches!(v.as_str(), "1" | "true" | "TRUE" | "yes" | "YES"))
        .unwrap_or(false)
}

fn parent_is_usable(state: &RoutingState, redis_url: &str, topic_gdp: GDPName, node: &str) -> bool {
    // Publisher root is always usable.
    if state.has_publisher(node) {
        return true;
    }
    // Only attach under proxies that have an active heartbeat. This prevents routing decisions
    // from placing subscribers under proxies that are registered but not yet ready/alive, which
    // can create large tail delays (no data flow until the proxy becomes healthy).
    if state.has_proxy(node) {
        return is_proxy_alive(redis_url, topic_gdp, node);
    }
    false
}

fn cas_backoff(attempt: usize) {
    // Deterministic bounded backoff to reduce thundering-herd CAS conflicts.
    let ms = (2u64.saturating_mul((attempt as u64) + 1)).min(50);
    std::thread::sleep(Duration::from_millis(ms));
}

fn now_ms_i64() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

fn log_state_summary(prefix: &str, state: &RoutingState) {
    if !routing_trace() {
        return;
    }
    let children = build_children_map(&state.edges);
    let f = fanout();
    let mut max_children = 0usize;
    let mut overfull: Vec<(String, usize)> = Vec::new();
    for (p, ch) in &children {
        max_children = max_children.max(ch.len());
        if ch.len() > f {
            overfull.push((p.clone(), ch.len()));
        }
    }
    overfull.sort_by(|a, b| b.1.cmp(&a.1));
    info!(
        "[routing][trace] {} publishers={} proxies={} edges={} parents={} max_children={} overfull_parents={:?}",
        prefix,
        state.publishers.len(),
        state.proxies.len(),
        state.edges.len(),
        children.len(),
        max_children,
        overfull
    );
}

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
    /// Best-effort timestamp (ms since epoch) when a node was (re)attached in the tree.
    /// Used to pick older children when doing disruptive graft operations.
    #[serde(default)]
    attach_ms: HashMap<String, i64>,
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
        self.edges
            .iter()
            .any(|e| e.parent == parent && e.child == child)
    }

    fn add_edge_unique(&mut self, parent: String, child: String) {
        if !self.has_edge(&parent, &child) {
            self.edges.push(Edge { parent, child });
        }
    }

    fn remove_edge(&mut self, parent: &str, child: &str) {
        self.edges
            .retain(|e| !(e.parent == parent && e.child == child));
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
        out.entry(e.parent.clone())
            .or_default()
            .push(e.child.clone());
    }
    // Deterministic order.
    for v in out.values_mut() {
        v.sort();
        v.dedup();
    }
    out
}

fn build_children_map_insertion_order(edges: &[Edge]) -> HashMap<String, Vec<String>> {
    // Preserve insertion order per parent (deduped). Useful for selecting "recent" children
    // during grafting to avoid repeatedly disrupting early-joining listeners.
    let mut out: HashMap<String, Vec<String>> = HashMap::new();
    let mut seen: HashMap<String, HashSet<String>> = HashMap::new();
    for e in edges {
        let set = seen.entry(e.parent.clone()).or_default();
        if set.insert(e.child.clone()) {
            out.entry(e.parent.clone())
                .or_default()
                .push(e.child.clone());
        }
    }
    out
}

fn bfs_intermediates(
    state: &RoutingState, root: &str, children: &HashMap<String, Vec<String>>,
) -> Vec<String> {
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

fn bfs_intermediates_with_depth(
    state: &RoutingState, root: &str, children: &HashMap<String, Vec<String>>,
) -> Vec<(String, usize)> {
    let mut out: Vec<(String, usize)> = Vec::new();
    let mut q: VecDeque<(String, usize)> = VecDeque::new();
    let mut seen = HashSet::<String>::new();

    q.push_back((root.to_string(), 0));
    seen.insert(root.to_string());

    while let Some((n, d)) = q.pop_front() {
        out.push((n.clone(), d));
        let Some(ch) = children.get(&n) else { continue };
        for c in ch {
            if state.is_intermediate(c) && seen.insert(c.clone()) {
                q.push_back((c.clone(), d + 1));
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

fn pick_unused_proxy(state: &RoutingState, redis_url: &str, topic_gdp: GDPName) -> Option<String> {
    let used = used_nodes_set(state);
    // Only pick proxies that are unused AND alive (have recent heartbeat)
    state
        .proxies
        .iter()
        .find(|p| !used.contains(*p) && is_proxy_alive(redis_url, topic_gdp, p))
        .cloned()
}

/// Register this node as a publisher for the topic.
pub fn register_publisher(
    redis_url: &str, topic_gdp: GDPName, publisher: GDPName, topic_name: &str,
) -> Result<(), String> {
    let key = routing_key(topic_gdp);
    for attempt in 0..MAX_RETRIES {
        let old = get_string(redis_url, &key)
            .unwrap_or(None)
            .unwrap_or_default();
        let mut st = parse_state(&old);
        let before = st.publishers.len();
        st.add_publisher(publisher.to_string());
        let after = st.publishers.len();

        let new = to_json(&st);
        if atomic_update(redis_url, &key, &new, &old, 1).map_err(|e| e.to_string())? {
            info!(
                "Registered publisher {} for topic {}",
                publisher, topic_name
            );
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
pub fn register_proxy(
    redis_url: &str, topic_gdp: GDPName, proxy: GDPName, topic_name: &str,
) -> Result<(), String> {
    let key = routing_key(topic_gdp);
    for attempt in 0..MAX_RETRIES {
        let old = get_string(redis_url, &key)
            .unwrap_or(None)
            .unwrap_or_default();
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
pub fn subscribe(
    redis_url: &str, topic_gdp: GDPName, topic_name: &str, subscriber: GDPName,
) -> Result<(), String> {
    let key = routing_key(topic_gdp);
    let subscriber_s = subscriber.to_string();

    // Benchmark hook: mark when this node begins routing subscribe attempts.
    // Uses container hostname as the stable per-node identifier.
    // Stored once (HSETNX) so retries don't overwrite.
    let _ = (|| -> Result<(), ()> {
        let client = redis::Client::open(redis_url).map_err(|_| ())?;
        let mut con = client.get_connection().map_err(|_| ())?;
        let ts_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| ())?
            .as_millis() as i64;
        let field = get_container_name();
        let _: i32 = redis::cmd("HSETNX")
            .arg("bench_join_attempt_ms")
            .arg(field)
            .arg(ts_ms)
            .query(&mut con)
            .map_err(|_| ())?;
        Ok(())
    })();

    for attempt in 0..MAX_RETRIES {
        let old = get_string(redis_url, &key)
            .unwrap_or(None)
            .unwrap_or_default();
        let mut st = parse_state(&old);

        log_state_summary("subscribe:loaded", &st);

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
            return Err(format!(
                "No publishers registered yet for topic {}",
                topic_name
            ));
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

        // Track (re)attachment time for churn-aware graft decisions.
        st.attach_ms
            .entry(subscriber_s.clone())
            .or_insert_with(now_ms_i64);
        // Seed attach timestamps for existing listener edges (best-effort). Without this,
        // older listeners can appear as "infinitely old" and be moved repeatedly during grafts,
        // inflating join-latency tails.
        let seed_ts = now_ms_i64();
        for e in &st.edges {
            if !st.is_intermediate(&e.child) {
                st.attach_ms.entry(e.child.clone()).or_insert(seed_ts);
            }
        }

        let children = build_children_map(&st.edges);
        let children_ins = build_children_map_insertion_order(&st.edges);
        let nodes_with_depth = bfs_intermediates_with_depth(&st, &root, &children);
        let nodes: Vec<String> = nodes_with_depth.iter().map(|(n, _)| n.clone()).collect();
        debug!(
            "[routing] subscribe topic={} subscriber={} bfs_nodes={:?}",
            topic_gdp, subscriber, nodes
        );

        // Pass 1: attach to first BFS node with spare fanout.
        for n in &nodes {
            if st.has_proxy(n) && !parent_is_usable(&st, redis_url, topic_gdp, n) {
                if routing_trace() {
                    info!(
                        "[routing][trace] subscribe skip_unusable_parent topic={} subscriber={} parent={} reason=proxy_not_alive",
                        topic_gdp, subscriber, n
                    );
                }
                continue;
            }
            let cnt = children.get(n).map(|v| v.len()).unwrap_or(0);
            let f = fanout();
            debug!(
                "[routing] subscribe topic={} subscriber={} pass=1 node={} children={} fanout={}",
                topic_gdp, subscriber, n, cnt, f
            );
            if cnt < f {
                info!(
                    "[routing] subscribe decision topic={} subscriber={} action=attach parent={} reason=capacity children={}/{}",
                    topic_gdp, subscriber, n, cnt, f
                );
                st.add_edge_unique(n.clone(), subscriber_s.clone());
                log_state_summary("subscribe:after_attach_mutation", &st);

                let new = to_json(&st);
                if atomic_update(redis_url, &key, &new, &old, 1).map_err(|e| e.to_string())? {
                    info!(
                        "Subscribed {} to topic {} under {}",
                        subscriber, topic_name, n
                    );
                    return Ok(());
                }
                debug!(
                    "[routing] subscribe topic={} subscriber={} action=attach parent={} result=cas_conflict retrying",
                    topic_gdp, subscriber, n
                );
                cas_backoff(attempt);
                continue;
            }
        }

        // Pass 2: no capacity anywhere => grow with a proxy graft.
        // We must keep fanout consistent for both the parent and the new proxy.
        // Prefer grafting deeper in the tree to minimize disruptive re-ordering of
        // high-level subtrees during mass joins.
        let mut candidates = nodes_with_depth.clone();
        candidates.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        for (n, depth) in candidates {
            if st.has_proxy(&n) && !parent_is_usable(&st, redis_url, topic_gdp, &n) {
                if routing_trace() {
                    info!(
                        "[routing][trace] subscribe pass=2 skip_unusable_parent topic={} subscriber={} parent={} depth={} reason=proxy_not_alive",
                        topic_gdp, subscriber, n, depth
                    );
                }
                continue;
            }
            let ch = children_ins.get(&n).cloned().unwrap_or_default();
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

            let Some(new_proxy) = pick_unused_proxy(&st, redis_url, topic_gdp) else {
                return Err(format!(
                    "No unused proxies available to grow routing tree for topic {}",
                    topic_name
                ));
            };

            // Parent `n` is full (otherwise pass-1 would have attached). After adding `new_proxy`,
            // we must move >= 1 child off `n`.
            // `new_proxy` must have <= fanout() children, and it must include `subscriber`,
            // so we may move at most fanout()-1 existing children.
            let mut non_proxy_children: Vec<String> =
                ch.iter().filter(|c| !st.is_proxy(c)).cloned().collect();
            let f = fanout();
            let move_count = non_proxy_children.len().min(f.saturating_sub(1)).max(1);
            // Prefer moving *older* children (those that have likely already received data)
            // to avoid inflating join latency for newly joining nodes.
            non_proxy_children.sort_by(|a, b| {
                let ta = st.attach_ms.get(a).copied().unwrap_or(0);
                let tb = st.attach_ms.get(b).copied().unwrap_or(0);
                ta.cmp(&tb).then_with(|| a.cmp(b))
            });
            non_proxy_children.truncate(move_count);

            info!(
                "[routing] subscribe decision topic={} subscriber={} action=graft_proxy parent={} depth={} new_proxy={} reason=no_capacity move_children_count={} children={:?}",
                topic_gdp, subscriber, n, depth, new_proxy, move_count, ch
            );
            if routing_trace() {
                info!(
                    "[routing][trace] graft plan topic={} subscriber={} parent={} depth={} new_proxy={} move_children={:?}",
                    topic_gdp, subscriber, n, depth, new_proxy, non_proxy_children
                );
            }

            // Add proxy under parent.
            st.add_edge_unique(n.clone(), new_proxy.clone());
            // Attach subscriber under proxy.
            st.add_edge_unique(new_proxy.clone(), subscriber_s.clone());
            // Move bounded set of non-proxy children under proxy.
            let ts = now_ms_i64();
            for c in non_proxy_children {
                let c2 = c.clone();
                st.remove_edge(&n, &c);
                st.add_edge_unique(new_proxy.clone(), c);
                // Mark moved child as "recent" so we don't keep moving the same nodes.
                st.attach_ms.insert(c2, ts);
            }
            log_state_summary("subscribe:after_graft_mutation", &st);

            let new = to_json(&st);
            if atomic_update(redis_url, &key, &new, &old, 1).map_err(|e| e.to_string())? {
                info!(
                    "Subscribed {} to topic {} via new proxy {}",
                    subscriber, topic_name, new_proxy
                );
                return Ok(());
            }
            debug!(
                "[routing] subscribe topic={} subscriber={} action=graft_proxy parent={} new_proxy={} result=cas_conflict retrying",
                topic_gdp, subscriber, n, new_proxy
            );
            cas_backoff(attempt);
        }

        warn!("Subscribe({}) could not find a place; retrying", subscriber);
        cas_backoff(attempt);
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
        let old = get_string(redis_url, &key)
            .unwrap_or(None)
            .unwrap_or_default();
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
        st.edges
            .retain(|e| !subtree.contains(&e.parent) && !subtree.contains(&e.child));
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
    let raw = get_string(redis_url, &key)
        .unwrap_or(None)
        .unwrap_or_default();
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
