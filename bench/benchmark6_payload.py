#!/usr/bin/env python3
"""
Benchmark 5: Payload Size
=========================

RESEARCH QUESTION:
    How does message size affect the bandwidth advantage of hierarchical routing?

HYPOTHESIS:
    - The bandwidth savings of hierarchical routing should persist at all sizes
    - Larger payloads will show the advantage more clearly (absolute Mbps saved)
    - Very large payloads may hit proxy throughput limits

WHAT WE MEASURE:
    - Publisher TX at different message sizes (1KB, 10KB, 100KB)
    - Compare hierarchical vs direct routing

WHY THIS MATTERS:
    Robots send diverse data: small sensor readings (1KB), compressed images (100KB),
    point clouds (1MB+). The routing advantage must hold across payload sizes.

EXPECTED RESULTS:
    - Both modes: TX increases with payload size
    - Hierarchical: Always lower TX than direct (constant factor of ~N/fanout)
"""
import json
import os
import time

from bench_utils import (
    PROJECT_DIR,
    PLOT_STYLE,
    choose_proxies,
    compose_up,
    docker,
    log,
    net_snapshot,
    net_delta,
    setup_plot_style,
    teardown,
    wait_for_listeners,
)

# -----------------------------------------------------------------------------
# Configuration
# -----------------------------------------------------------------------------
OUT_DIR = os.path.join(PROJECT_DIR, "bench", "results")
OUT_JSON = os.path.join(OUT_DIR, "benchmark6_payload.json")
OUT_PNG = os.path.join(OUT_DIR, "benchmark6_payload.png")

PAYLOAD_SIZES_KB = [1, 10, 100]   # Message sizes to test
SUBSCRIBERS = 20                  # Fixed subscriber count
FANOUT = 3
MEASURE_SECS = 15
WARMUP_SECS = 5
TIMEOUT_SECS = 120
MSG_HZ = 30                       # Lower rate for larger payloads


# -----------------------------------------------------------------------------
# Benchmark Logic
# -----------------------------------------------------------------------------
def run_single(docker_cli, mode: str, payload_kb: int) -> dict:
    """Run one test case."""
    
    payload_bytes = payload_kb * 1024
    
    if mode == "direct":
        fanout, proxies = 1000, 0
    else:
        fanout = FANOUT
        proxies = choose_proxies(SUBSCRIBERS, fanout)
    
    log(f"\n=== {mode} | payload={payload_kb}KB ===")
    
    os.environ["FANOUT_FACTOR"] = str(fanout)
    os.environ["BENCH_LATENCY"] = "1"
    os.environ["BENCH_LATENCY_HZ"] = str(MSG_HZ)
    os.environ["BENCH_PAYLOAD_BYTES"] = str(payload_bytes)
    
    teardown(docker_cli, volumes=True)
    compose_up(docker_cli, listeners=SUBSCRIBERS, proxies=proxies, env=None)
    
    if not wait_for_listeners(SUBSCRIBERS, TIMEOUT_SECS):
        teardown(docker_cli, volumes=True)
        return {"success": False, "error": "timeout"}
    
    log(f"  warmup {WARMUP_SECS}s...")
    time.sleep(WARMUP_SECS)
    
    log(f"  measuring for {MEASURE_SECS}s...")
    snap_before = net_snapshot(docker_cli)
    time.sleep(MEASURE_SECS)
    snap_after = net_snapshot(docker_cli)
    
    delta = net_delta(snap_before, snap_after)
    
    publisher_tx = sum(
        v.get("tx", 0) for v in delta.values()
        if v and v.get("service") == "talker"
    )
    publisher_tx_mbps = (publisher_tx * 8) / (MEASURE_SECS * 1e6)
    
    log(f"  publisher TX: {publisher_tx_mbps:.2f} Mbps")
    
    teardown(docker_cli, volumes=True)
    
    return {
        "success": True,
        "mode": mode,
        "payload_kb": payload_kb,
        "subscribers": SUBSCRIBERS,
        "publisher_tx_mbps": publisher_tx_mbps,
    }


def run_benchmark():
    docker_cli = docker()
    results = {"runs": [], "config": {
        "payload_sizes_kb": PAYLOAD_SIZES_KB,
        "subscribers": SUBSCRIBERS,
        "fanout": FANOUT,
        "msg_hz": MSG_HZ,
    }}
    
    for size in PAYLOAD_SIZES_KB:
        results["runs"].append(run_single(docker_cli, "hierarchical", size))
        results["runs"].append(run_single(docker_cli, "direct", size))
        with open(OUT_JSON, "w") as f:
            json.dump(results, f, indent=2)
    
    return results


# -----------------------------------------------------------------------------
# Plotting
# -----------------------------------------------------------------------------
def plot(results):
    import matplotlib.pyplot as plt
    
    setup_plot_style()
    colors = PLOT_STYLE["colors"]
    width = PLOT_STYLE["bar_width"]
    
    hier = {r["payload_kb"]: r for r in results["runs"] 
            if r.get("success") and r["mode"] == "hierarchical"}
    direct = {r["payload_kb"]: r for r in results["runs"] 
              if r.get("success") and r["mode"] == "direct"}
    
    sizes = sorted(set(hier.keys()) & set(direct.keys()))
    if not sizes:
        return
    
    fig, ax = plt.subplots()
    
    labels = [f"{s}KB" for s in sizes]
    x = list(range(len(sizes)))
    
    direct_tx = [direct[s]["publisher_tx_mbps"] for s in sizes]
    hier_tx = [hier[s]["publisher_tx_mbps"] for s in sizes]
    
    ax.bar([i - width/2 for i in x], direct_tx, width, 
           label="Direct", color=colors["direct"])
    ax.bar([i + width/2 for i in x], hier_tx, width, 
           label="Hierarchical", color=colors["hierarchical"])
    
    ax.set_xlabel("Payload Size")
    ax.set_ylabel("Publisher Bandwidth (Mbps)")
    ax.set_title(f"Publisher Bandwidth vs Message Size ({SUBSCRIBERS} subscribers)")
    ax.set_xticks(x)
    ax.set_xticklabels(labels)
    ax.legend()
    ax.set_ylim(bottom=0)
    
    fig.tight_layout()
    fig.savefig(OUT_PNG)
    plt.close(fig)
    log(f"wrote: {OUT_PNG}")


def validate(results):
    if not results.get("runs"):
        return False
    need = {(s, m) for s in PAYLOAD_SIZES_KB for m in ["hierarchical", "direct"]}
    have = {(r["payload_kb"], r["mode"]) for r in results["runs"] if r.get("success")}
    return need == have


def main():
    log("=" * 60)
    log("BENCHMARK 5: PAYLOAD SIZE")
    log("Question: Does the bandwidth advantage hold for large messages?")
    log("=" * 60)
    
    os.makedirs(OUT_DIR, exist_ok=True)
    
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

