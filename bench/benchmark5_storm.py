#!/usr/bin/env python3
"""
Benchmark 5: Storm Resilience
Measures throughput during mass disconnection and recovery.
"""
import json
import os
import time
from collections import defaultdict
from random import Random

from bench_utils import (
    PROJECT_DIR, PLOT_STYLE, choose_proxies, connected_listener_count,
    docker, log, net_snapshot, net_delta, rds, service_of, setup_plot_style, teardown,
)

OUT_DIR = os.path.join(PROJECT_DIR, "bench", "results")
OUT_JSON = os.path.join(OUT_DIR, "benchmark5_storm.json")
OUT_PNG = os.path.join(OUT_DIR, "benchmark5_throughput.png")
OUT_TREE_BEFORE = os.path.join(OUT_DIR, "benchmark5_tree_before.png")
OUT_TREE_AFTER = os.path.join(OUT_DIR, "benchmark5_tree_after.png")

SUBSCRIBERS = 30
FANOUT = 3
KILL_LISTENER_PCT = 50
KILL_PROXY_PCT = 30
MSG_HZ = 50

BASELINE_SECS = 45
STORM_SECS = 30
RECOVERY_SECS = 60
TIMEOUT_SECS = 180


def get_tree():
    """Get tree structure from Redis."""
    try:
        r = rds()
        keys = r.keys("*-routing")
        if not keys:
            r.close()
            return None
        raw = r.get(keys[0])
        r.close()
        if not raw:
            return None
        
        st = json.loads(raw)
        pubs = st.get("publishers", []) or []
        proxies = set(st.get("proxies", []) or [])
        edges = st.get("edges", []) or []
        if not pubs:
            return None
        
        root = pubs[0]
        children = defaultdict(list)
        for e in edges:
            if e.get("parent") and e.get("child"):
                children[e["parent"]].append(e["child"])
        
        levels = defaultdict(list)
        node_type = {}
        dist = {root: 0}
        q = [root]
        while q:
            n = q.pop(0)
            node_type[n] = "publisher" if n in pubs else ("proxy" if n in proxies else "listener")
            levels[dist[n]].append(n)
            for c in children.get(n, []):
                if c not in dist:
                    dist[c] = dist[n] + 1
                    q.append(c)
        
        return {"root": root, "levels": dict(levels), "children": dict(children),
                "node_type": node_type, "depth": max(levels.keys()) if levels else 0}
    except Exception as e:
        log(f"  tree error: {e}")
        return None


def plot_tree(tree, title, path):
    """Simple tree visualization."""
    import matplotlib.pyplot as plt
    import matplotlib.patches as mpatches
    
    if not tree:
        return
    setup_plot_style()
    
    levels, children, node_type = tree["levels"], tree["children"], tree["node_type"]
    colors = {"publisher": "#2CA02C", "proxy": "#1F77B4", "listener": "#FF7F0E"}
    
    fig, ax = plt.subplots(figsize=(12, 6))
    max_w = max(len(v) for v in levels.values())
    pos = {}
    
    for d, nodes in levels.items():
        for i, n in enumerate(nodes):
            pos[n] = ((i + 1) * max_w / (len(nodes) + 1), -d)
    
    for p, cs in children.items():
        if p in pos:
            for c in cs:
                if c in pos:
                    ax.plot([pos[p][0], pos[c][0]], [pos[p][1], pos[c][1]], "-", color="#ccc", lw=1)
    
    for n, (x, y) in pos.items():
        t = node_type.get(n, "listener")
        ax.scatter([x], [y], c=[colors.get(t, "#999")], s=200 if t != "listener" else 120,
                   marker="s" if t == "publisher" else ("D" if t == "proxy" else "o"),
                   edgecolors="white", linewidths=1, zorder=2)
    
    ax.legend(handles=[mpatches.Patch(color=c, label=l.title()) for l, c in colors.items()], loc="upper right")
    ax.axis("off")
    ax.set_title(title)
    fig.tight_layout()
    fig.savefig(path, bbox_inches="tight")
    plt.close(fig)
    log(f"wrote: {path}")


def run():
    docker_cli = docker()
    proxies = choose_proxies(SUBSCRIBERS, FANOUT)
    
    log(f"Storm: {SUBSCRIBERS} subs, kill {KILL_LISTENER_PCT}% listeners + {KILL_PROXY_PCT}% proxies")
    
    os.environ["FANOUT_FACTOR"] = str(FANOUT)
    os.environ["BENCH_LATENCY"] = "1"
    os.environ["BENCH_LATENCY_HZ"] = str(MSG_HZ)
    
    teardown(docker_cli, volumes=True)
    docker_cli.compose.up(detach=True, scales={"listener": SUBSCRIBERS, "proxy": proxies})
    
    # Wait for connections
    start = time.time()
    while time.time() - start < TIMEOUT_SECS:
        n = connected_listener_count()
        if n >= SUBSCRIBERS:
            break
        log(f"  {n}/{SUBSCRIBERS} connected...")
        time.sleep(3)
    else:
        teardown(docker_cli, volumes=True)
        raise SystemExit("timeout")
    
    log(f"  all connected")
    time.sleep(5)
    
    tree_before = get_tree()
    
    # Select victims (listeners and proxies)
    rng = Random(42)
    cache = {}
    containers = docker_cli.compose.ps()
    listeners = [c.id for c in containers if service_of(docker_cli, c, cache) == "listener"]
    proxies_list = [c.id for c in containers if service_of(docker_cli, c, cache) == "proxy"]
    
    rng.shuffle(listeners)
    rng.shuffle(proxies_list)
    
    victim_listeners = listeners[:max(1, int(KILL_LISTENER_PCT / 100 * len(listeners)))]
    victim_proxies = proxies_list[:max(0, int(KILL_PROXY_PCT / 100 * len(proxies_list)))]
    victims = victim_listeners + victim_proxies
    
    log(f"  victims: {len(victim_listeners)} listeners + {len(victim_proxies)} proxies")
    
    samples, events = [], []
    t0 = time.time()
    prev_snap = net_snapshot(docker_cli)
    prev_t = t0
    
    def sample(phase):
        nonlocal prev_snap, prev_t
        now = time.time()
        curr = net_snapshot(docker_cli)
        delta = net_delta(prev_snap, curr)
        elapsed = now - prev_t
        rx = sum(v.get("rx", 0) for v in delta.values() if v and v.get("service") == "listener")
        mbps = (rx * 8) / (elapsed * 1e6) if elapsed > 0 else 0
        if mbps > 10000:
            mbps = 0  # Sanity cap
        conn = connected_listener_count()
        samples.append({"t": now - t0, "phase": phase, "rx_mbps": mbps, "connected": conn})
        prev_snap, prev_t = curr, now
        return mbps, conn
    
    # Baseline
    log("BASELINE...")
    end = t0 + BASELINE_SECS
    while time.time() < end:
        mbps, conn = sample("baseline")
        log(f"  t={time.time()-t0:.0f}s: {mbps:.1f} Mbps, {conn} conn")
    
    baseline = [s["rx_mbps"] for s in samples if s["phase"] == "baseline" and s["rx_mbps"] > 0]
    baseline_avg = sum(baseline) / len(baseline) if baseline else 0
    log(f"  baseline avg: {baseline_avg:.1f} Mbps")
    
    # Storm
    log(f"STORM - killing {len(victim_listeners)} listeners + {len(victim_proxies)} proxies...")
    events.append({"t": time.time() - t0, "event": "storm_start", 
                   "killed_listeners": len(victim_listeners), "killed_proxies": len(victim_proxies)})
    for v in victims:
        try:
            docker_cli.container.kill(v)
        except:
            pass
    
    end = time.time() + STORM_SECS
    while time.time() < end:
        mbps, conn = sample("storm")
        log(f"  t={time.time()-t0:.0f}s: {mbps:.1f} Mbps, {conn} conn")
    
    # Recovery
    log(f"RECOVERY - restarting {len(victims)} containers...")
    events.append({"t": time.time() - t0, "event": "recovery_start"})
    for v in victims:
        try:
            docker_cli.container.start(v)
        except:
            pass
    
    end = time.time() + RECOVERY_SECS
    while time.time() < end:
        mbps, conn = sample("recovery")
        log(f"  t={time.time()-t0:.0f}s: {mbps:.1f} Mbps, {conn} conn")
    
    # DEBUG: Check per-listener RX at end of recovery
    log("  --- per-listener breakdown (last sample period) ---")
    snap1 = net_snapshot(docker_cli)
    time.sleep(10)
    snap2 = net_snapshot(docker_cli)
    delta = net_delta(snap1, snap2)
    receiving, silent_containers = 0, []
    for name, v in sorted(delta.items()):
        if v.get("service") == "listener":
            rx_kb = v.get("rx", 0) / 1024
            if rx_kb > 10:
                receiving += 1
            else:
                silent_containers.append(name)
                log(f"    SILENT: {name} rx={rx_kb:.1f}KB")
    log(f"  {receiving} receiving, {len(silent_containers)} silent (should be 0)")
    
    # Dump ALL container logs to files for analysis
    log("  --- SAVING ALL CONTAINER LOGS ---")
    import subprocess
    log_dir = os.path.join(OUT_DIR, "storm_logs")
    os.makedirs(log_dir, exist_ok=True)
    for c in docker_cli.compose.ps():
        name = c.name
        try:
            result = subprocess.run(
                ["docker", "logs", name],
                capture_output=True, text=True, timeout=30
            )
            with open(os.path.join(log_dir, f"{name}.log"), "w") as f:
                f.write(f"=== STDOUT ===\n{result.stdout}\n=== STDERR ===\n{result.stderr}")
        except Exception as e:
            log(f"    (error getting logs for {name}: {e})")
    log(f"  logs saved to {log_dir}/")
    
    # Show summary of silent listeners
    if silent_containers:
        log(f"  --- SILENT LISTENERS: {silent_containers} ---")
    log("  ---")
    
    tree_after = get_tree()
    
    # Check if recovery reached baseline
    recovery = [s for s in samples if s["phase"] == "recovery"]
    last_5 = recovery[-5:] if len(recovery) >= 5 else recovery
    recovery_avg = sum(s["rx_mbps"] for s in last_5) / len(last_5) if last_5 else 0
    recovery_pct = (recovery_avg / baseline_avg * 100) if baseline_avg > 0 else 0
    log(f"  recovery avg (last 5): {recovery_avg:.1f} Mbps ({recovery_pct:.0f}% of baseline)")
    
    teardown(docker_cli, volumes=True)
    
    return {
        "config": {"subscribers": SUBSCRIBERS, "fanout": FANOUT, 
                   "kill_listener_pct": KILL_LISTENER_PCT, "kill_proxy_pct": KILL_PROXY_PCT,
                   "msg_hz": MSG_HZ, "baseline_secs": BASELINE_SECS, "storm_secs": STORM_SECS,
                   "recovery_secs": RECOVERY_SECS},
        "samples": samples, "events": events,
        "tree_before": tree_before, "tree_after": tree_after,
        "summary": {"baseline_rx_mbps": baseline_avg, 
                    "killed_listeners": len(victim_listeners), "killed_proxies": len(victim_proxies),
                    "recovery_avg_mbps": recovery_avg, "recovery_pct": recovery_pct,
                    "total_samples": len(samples)},
    }


def plot(results):
    import matplotlib.pyplot as plt
    setup_plot_style()
    
    samples = results.get("samples", [])
    events = results.get("events", [])
    summary = results.get("summary", {})
    if not samples:
        return
    
    fig, ax = plt.subplots(figsize=(12, 5))
    t = [s["t"] for s in samples]
    rx = [s["rx_mbps"] for s in samples]
    conn = [s["connected"] for s in samples]
    
    ax.plot(t, rx, "-", color=PLOT_STYLE["colors"]["hierarchical"], lw=1.5, label="Throughput")
    ax.fill_between(t, rx, alpha=0.15, color=PLOT_STYLE["colors"]["hierarchical"])
    
    ax2 = ax.twinx()
    ax2.step(t, conn, where="post", color=PLOT_STYLE["colors"]["neutral"], lw=1.5, alpha=0.7, label="Connected")
    ax2.set_ylabel("Connected", color=PLOT_STYLE["colors"]["neutral"])
    
    for ev in events:
        c = PLOT_STYLE["colors"]["direct"] if ev["event"] == "storm_start" else "#2CA02C"
        ax.axvline(ev["t"], color=c, ls="--", lw=2, label=ev["event"].replace("_", " ").title())
    
    if summary.get("baseline_rx_mbps"):
        ax.axhline(summary["baseline_rx_mbps"], color=PLOT_STYLE["colors"]["neutral"], ls=":", lw=1.5, alpha=0.6, label="Baseline")
    
    ax.set_xlabel("Time (s)")
    ax.set_ylabel("Throughput (Mbps)")
    ax.set_title(f"Storm: {summary.get('recovery_pct', 0):.0f}% recovery")
    ax.legend(loc="lower right")
    ax.set_ylim(bottom=0)
    
    fig.tight_layout()
    fig.savefig(OUT_PNG)
    plt.close(fig)
    log(f"wrote: {OUT_PNG}")


def main():
    os.makedirs(OUT_DIR, exist_ok=True)
    
    if os.path.exists(OUT_JSON):
        try:
            with open(OUT_JSON) as f:
                cached = json.load(f)
            if len(cached.get("samples", [])) >= 20:
                log(f"Using cached: {OUT_JSON}")
                plot(cached)
                if cached.get("tree_before"):
                    plot_tree(cached["tree_before"], "Tree Before Storm", OUT_TREE_BEFORE)
                if cached.get("tree_after"):
                    plot_tree(cached["tree_after"], "Tree After Recovery", OUT_TREE_AFTER)
                return
        except:
            pass
    
    results = run()
    with open(OUT_JSON, "w") as f:
        json.dump(results, f, indent=2)
    log(f"wrote: {OUT_JSON}")
    plot(results)
    if results.get("tree_before"):
        plot_tree(results["tree_before"], "Tree Before Storm", OUT_TREE_BEFORE)
    if results.get("tree_after"):
        plot_tree(results["tree_after"], "Tree After Recovery", OUT_TREE_AFTER)


if __name__ == "__main__":
    main()
