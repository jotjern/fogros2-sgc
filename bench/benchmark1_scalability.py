#!/usr/bin/env python3
"""
Benchmark 1: Scalability
========================

RESEARCH QUESTION:
    Does hierarchical routing reduce publisher bandwidth as subscriber count increases?

HYPOTHESIS:
    - Direct: Publisher TX scales O(N) with subscriber count
    - Hierarchical: Publisher TX stays O(fanout), independent of N

WHAT WE MEASURE:
    - Publisher network TX (bytes sent by talker container)
    
WHY THIS MATTERS:
    A robot streaming camera feeds to many observers should not be bottlenecked
    by its uplink bandwidth. Hierarchical routing offloads fan-out to proxies.

EXPECTED RESULTS:
    - Direct mode: Linear increase in publisher TX as N grows
    - Hierarchical mode: Flat or near-flat publisher TX regardless of N
"""
import json
import os
import time

from bench_utils import (
    PROJECT_DIR,
    choose_proxies,
    compose_up,
    docker,
    log,
    net_snapshot,
    net_delta,
    teardown,
    wait_for_listeners,
)

# -----------------------------------------------------------------------------
# Configuration
# -----------------------------------------------------------------------------
OUT_DIR = os.path.join(PROJECT_DIR, "bench", "results")
OUT_JSON = os.path.join(OUT_DIR, "benchmark1_scalability.json")
OUT_PNG = os.path.join(OUT_DIR, "benchmark1_scalability.png")

SUBSCRIBER_COUNTS = [1, 5, 10, 20, 30, 40, 50]
FANOUT = 3                # Hierarchical fanout factor
MEASURE_SECS = 20         # Measurement duration
WARMUP_SECS = 5           # Stabilization before measurement
TIMEOUT_SECS = 180        # Max wait for subscribers to connect


# -----------------------------------------------------------------------------
# Benchmark Logic
# -----------------------------------------------------------------------------
def run_single(docker_cli, mode: str, n_subscribers: int) -> dict:
    """Run one test case and return results."""
    
    # Setup
    if mode == "direct":
        fanout, proxies = 1000, 0  # High fanout = direct to all subscribers
    else:
        fanout = FANOUT
        proxies = choose_proxies(n_subscribers, fanout)
    
    log(f"\n=== {mode} | subscribers={n_subscribers} | proxies={proxies} ===")
    
    os.environ["FANOUT_FACTOR"] = str(fanout)
    
    # Start containers
    teardown(docker_cli, volumes=True)
    compose_up(docker_cli, listeners=n_subscribers, proxies=proxies, env=None)
    
    # Wait for all subscribers to connect
    if not wait_for_listeners(n_subscribers, TIMEOUT_SECS):
        log("  ERROR: subscribers did not connect")
        teardown(docker_cli, volumes=True)
        return {"success": False, "error": "connection timeout"}
    
    # Warmup
    log(f"  warmup {WARMUP_SECS}s...")
    time.sleep(WARMUP_SECS)
    
    # Measure publisher TX
    log(f"  measuring for {MEASURE_SECS}s...")
    snap_before = net_snapshot(docker_cli)
    time.sleep(MEASURE_SECS)
    snap_after = net_snapshot(docker_cli)
    
    delta = net_delta(snap_before, snap_after)
    publisher_tx_bytes = sum(
        v.get("tx", 0) for v in delta.values() 
        if v and v.get("service") == "talker"
    )
    publisher_tx_mbps = (publisher_tx_bytes * 8) / (MEASURE_SECS * 1e6)
    
    log(f"  publisher TX: {publisher_tx_mbps:.2f} Mbps")
    
    # Cleanup
    teardown(docker_cli, volumes=True)
    
    return {
        "success": True,
        "mode": mode,
        "subscribers": n_subscribers,
        "proxies": proxies,
        "publisher_tx_mbps": publisher_tx_mbps,
    }


def run_benchmark():
    """Run complete benchmark."""
    docker_cli = docker()
    results = {"runs": [], "config": {
        "subscriber_counts": SUBSCRIBER_COUNTS,
        "fanout": FANOUT,
        "measure_secs": MEASURE_SECS,
    }}
    
    for n in SUBSCRIBER_COUNTS:
        results["runs"].append(run_single(docker_cli, "hierarchical", n))
        results["runs"].append(run_single(docker_cli, "direct", n))
        
        # Save incrementally
        with open(OUT_JSON, "w") as f:
            json.dump(results, f, indent=2)
    
    return results


# -----------------------------------------------------------------------------
# Plotting
# -----------------------------------------------------------------------------
def plot(results):
    import matplotlib.pyplot as plt
    from bench_utils import PLOT_STYLE, setup_plot_style
    
    setup_plot_style()
    colors = PLOT_STYLE["colors"]
    width = PLOT_STYLE["bar_width"]
    
    hier = {r["subscribers"]: r for r in results["runs"] 
            if r.get("success") and r["mode"] == "hierarchical"}
    direct = {r["subscribers"]: r for r in results["runs"] 
              if r.get("success") and r["mode"] == "direct"}
    
    xs = sorted(set(hier.keys()) & set(direct.keys()))
    if not xs:
        log("  ERROR: no data to plot")
        return
    
    fig, ax = plt.subplots()
    
    x_pos = list(range(len(xs)))
    direct_tx = [direct[n]["publisher_tx_mbps"] for n in xs]
    hier_tx = [hier[n]["publisher_tx_mbps"] for n in xs]
    
    ax.bar([p - width/2 for p in x_pos], direct_tx, width, 
           label="Direct", color=colors["direct"])
    ax.bar([p + width/2 for p in x_pos], hier_tx, width, 
           label="Hierarchical", color=colors["hierarchical"])
    
    ax.set_xlabel("Number of Subscribers")
    ax.set_ylabel("Publisher Bandwidth (Mbps)")
    ax.set_title("Required Publisher Bandwidth")
    ax.legend()
    ax.set_xticks(x_pos)
    ax.set_xticklabels([str(x) for x in xs])
    ax.set_ylim(bottom=0)
    
    fig.tight_layout()
    fig.savefig(OUT_PNG)
    plt.close(fig)
    log(f"wrote: {OUT_PNG}")


# -----------------------------------------------------------------------------
# Validation & Main
# -----------------------------------------------------------------------------
def validate(results):
    if not results.get("runs"):
        return False
    need = {(n, m) for n in SUBSCRIBER_COUNTS for m in ["hierarchical", "direct"]}
    have = {(r["subscribers"], r["mode"]) for r in results["runs"] if r.get("success")}
    return need == have


def main():
    log("=" * 60)
    log("BENCHMARK 1: SCALABILITY")
    log("Question: Does hierarchical routing reduce publisher bandwidth?")
    log("=" * 60)
    
    os.makedirs(OUT_DIR, exist_ok=True)
    
    # Try cached results
    if os.path.exists(OUT_JSON):
        try:
            with open(OUT_JSON) as f:
                cached = json.load(f)
            if validate(cached):
                log(f"Using cached: {OUT_JSON}")
                plot(cached)
                return
        except Exception:
            pass
    
    results = run_benchmark()
    plot(results)
    log(f"wrote: {OUT_JSON}")


if __name__ == "__main__":
    main()

