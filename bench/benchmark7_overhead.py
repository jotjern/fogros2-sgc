#!/usr/bin/env python3
"""
Benchmark 7: Protocol Overhead
==============================

RESEARCH QUESTION:
    How much overhead does the hierarchical routing add compared to ideal?
    What is the effective goodput vs. raw throughput?

WHAT WE MEASURE:
    - Goodput: actual application data delivered per second
    - Raw throughput: total bytes transmitted (including headers, WebRTC, etc.)
    - Overhead ratio: (raw - goodput) / raw

METHODOLOGY:
    1. Send known-size messages at known rate
    2. Measure actual network bytes transferred (TX from talker, RX at listeners)
    3. Compare to theoretical goodput: msg_size * msg_rate * num_listeners

    Overhead sources:
    - ROS2 serialization overhead
    - GDP packet headers
    - WebRTC DTLS/SCTP framing
    - Proxy store-and-forward

EXPECTED RESULTS:
    - Direct mode: Lower overhead (single hop)
    - Hierarchical: Higher overhead (multiple hops, proxy processing)
    - Overhead should increase with tree depth
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
    measure_net,
    setup_plot_style,
    teardown,
    wait_for_listeners,
    warm_up,
)

# -----------------------------------------------------------------------------
# Configuration
# -----------------------------------------------------------------------------
OUT_DIR = os.path.join(PROJECT_DIR, "bench", "results")
OUT_JSON = os.path.join(OUT_DIR, "benchmark7_overhead.json")
OUT_PNG = os.path.join(OUT_DIR, "benchmark7_overhead.png")

SUBSCRIBERS = 20
FANOUT = 3
PAYLOAD_KB = 10  # Known payload size
MSG_HZ = 20      # Known message rate
MEASURE_SECS = 30
WARMUP_SECS = 10
TIMEOUT_SECS = 120


# -----------------------------------------------------------------------------
# Benchmark Logic
# -----------------------------------------------------------------------------
def run_single(docker_cli, mode: str) -> dict:
    """Run one test case."""
    
    if mode == "direct":
        fanout, proxies = 1000, 0
    else:
        fanout = FANOUT
        proxies = choose_proxies(SUBSCRIBERS, fanout)
    
    log(f"\n=== {mode} | subscribers={SUBSCRIBERS} | proxies={proxies} ===")
    
    os.environ["FANOUT_FACTOR"] = str(fanout)
    os.environ["BENCH_LATENCY"] = "1"
    os.environ["BENCH_LATENCY_HZ"] = str(MSG_HZ)
    os.environ["BENCH_PAYLOAD_BYTES"] = str(PAYLOAD_KB * 1024)
    
    teardown(docker_cli, volumes=True)
    compose_up(docker_cli, listeners=SUBSCRIBERS, proxies=proxies, env=None)
    
    if not wait_for_listeners(SUBSCRIBERS, TIMEOUT_SECS):
        teardown(docker_cli, volumes=True)
        return {"success": False, "error": "timeout"}
    
    warm_up(WARMUP_SECS)
    
    log(f"  measuring for {MEASURE_SECS}s...")
    delta, talker_tx_mbps, listener_rx_mbps = measure_net(docker_cli, MEASURE_SECS)
    
    # DEBUG: show what we counted
    log(f"  --- per-container breakdown ---")
    listener_count = 0
    for name, v in sorted(delta.items()):
        svc = v.get("service", "?")
        rx_mb = v.get("rx", 0) / 1e6
        tx_mb = v.get("tx", 0) / 1e6
        if svc == "listener":
            listener_count += 1
            log(f"    {name}: svc={svc} rx={rx_mb:.2f}MB tx={tx_mb:.2f}MB")
    log(f"  counted {listener_count} listeners")
    log(f"  ---")
    
    # Calculate theoretical goodput
    # Payload * msg_rate * subscribers (what listeners should receive in aggregate)
    theoretical_goodput_mbps = (PAYLOAD_KB * 1024 * MSG_HZ * SUBSCRIBERS * 8) / 1e6
    
    # Publisher theoretical TX (to first hop)
    # In hierarchical: publisher sends to fanout children
    # In direct: publisher sends to all subscribers
    if mode == "direct":
        pub_theoretical_tx = (PAYLOAD_KB * 1024 * MSG_HZ * SUBSCRIBERS * 8) / 1e6
    else:
        # Hierarchical: publisher only sends to first level of tree
        pub_theoretical_tx = (PAYLOAD_KB * 1024 * MSG_HZ * min(SUBSCRIBERS, fanout) * 8) / 1e6
    
    # Calculate overhead
    overhead_ratio = 0.0
    if listener_rx_mbps > 0 and theoretical_goodput_mbps > 0:
        # Overhead = how much more we receive than payload alone
        overhead_ratio = max(0, (listener_rx_mbps - theoretical_goodput_mbps) / listener_rx_mbps)
    
    log(f"  talker TX:     {talker_tx_mbps:.2f} Mbps")
    log(f"  listener RX:   {listener_rx_mbps:.2f} Mbps (aggregate)")
    log(f"  theoretical:   {theoretical_goodput_mbps:.2f} Mbps")
    log(f"  overhead:      {overhead_ratio*100:.1f}%")
    
    teardown(docker_cli, volumes=True)
    
    return {
        "success": True,
        "mode": mode,
        "subscribers": SUBSCRIBERS,
        "proxies": proxies,
        "payload_kb": PAYLOAD_KB,
        "msg_hz": MSG_HZ,
        "talker_tx_mbps": talker_tx_mbps,
        "listener_rx_mbps": listener_rx_mbps,
        "theoretical_goodput_mbps": theoretical_goodput_mbps,
        "overhead_ratio": overhead_ratio,
    }


def run_benchmark():
    docker_cli = docker()
    results = {"runs": [], "config": {
        "subscribers": SUBSCRIBERS,
        "fanout": FANOUT,
        "payload_kb": PAYLOAD_KB,
        "msg_hz": MSG_HZ,
        "measure_secs": MEASURE_SECS,
    }}
    
    for mode in ["hierarchical", "direct"]:
        results["runs"].append(run_single(docker_cli, mode))
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
    
    runs = {r["mode"]: r for r in results["runs"] if r.get("success")}
    if not runs:
        return
    
    modes = ["direct", "hierarchical"]
    modes = [m for m in modes if m in runs]
    
    fig, (ax1, ax2) = plt.subplots(1, 2, figsize=(12, 5))
    
    # Left plot: Throughput comparison
    x = range(len(modes))
    width = 0.35
    
    theoretical = [runs[m]["theoretical_goodput_mbps"] for m in modes]
    actual_rx = [runs[m]["listener_rx_mbps"] for m in modes]
    talker_tx = [runs[m]["talker_tx_mbps"] for m in modes]
    
    ax1.bar([i - width/2 for i in x], theoretical, width, label="Theoretical Goodput", color="#2CA02C", alpha=0.8)
    ax1.bar([i + width/2 for i in x], actual_rx, width, label="Actual Listener RX", color=colors["hierarchical"], alpha=0.8)
    
    ax1.set_xticks(x)
    ax1.set_xticklabels([m.capitalize() for m in modes])
    ax1.set_ylabel("Throughput (Mbps)")
    ax1.set_title("Aggregate Listener Throughput")
    ax1.legend()
    ax1.set_ylim(bottom=0)
    
    # Right plot: Publisher TX comparison
    mode_colors = [colors["direct"] if m == "direct" else colors["hierarchical"] for m in modes]
    bars = ax2.bar(x, talker_tx, color=mode_colors, alpha=0.8)
    
    ax2.set_xticks(x)
    ax2.set_xticklabels([m.capitalize() for m in modes])
    ax2.set_ylabel("Publisher TX (Mbps)")
    ax2.set_title("Publisher Bandwidth Usage")
    ax2.set_ylim(bottom=0)
    
    # Add overhead percentage labels
    for i, m in enumerate(modes):
        overhead = runs[m]["overhead_ratio"] * 100
        ax2.annotate(f"Overhead: {overhead:.0f}%", 
                     xy=(i, talker_tx[i]), xytext=(0, 5),
                     textcoords="offset points", ha="center", fontsize=9)
    
    fig.tight_layout()
    fig.savefig(OUT_PNG)
    plt.close(fig)
    log(f"wrote: {OUT_PNG}")


def validate(results):
    if not results.get("runs"):
        return False
    need = {"hierarchical", "direct"}
    have = {r["mode"] for r in results["runs"] if r.get("success")}
    return need == have


def main():
    log("=" * 60)
    log("BENCHMARK 7: PROTOCOL OVERHEAD")
    log("Question: How much overhead does hierarchical routing add?")
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

