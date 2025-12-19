#!/usr/bin/env python3
"""
Benchmark 4: Fanout Tuning
==========================

RESEARCH QUESTION:
    What fanout value gives the best latency-cost tradeoff?

HYPOTHESIS:
    - Higher fanout = shallower tree = lower latency, but more direct connections
    - Lower fanout = deeper tree = higher latency, but better load distribution
    - Optimal fanout depends on subscriber count and latency requirements

WHAT WE MEASURE:
    - End-to-end latency at different fanout values
    - Number of proxies required (infrastructure cost)
    - Tree depth (derived)

WHY THIS MATTERS:
    Operators need to tune fanout for their deployment. This benchmark provides
    data to inform that decision.

EXPECTED RESULTS:
    - Latency decreases as fanout increases (fewer hops)
    - Proxy count decreases as fanout increases (flatter tree)
    - Diminishing returns above fanout ~5-10
"""
import json
import math
import os
import time

from bench_utils import (
    PROJECT_DIR,
    choose_proxies,
    compose_up,
    docker,
    log,
    mean_std,
    parse_latency_samples_from_logs,
    percentile,
    teardown,
    wait_for_listeners,
)

# -----------------------------------------------------------------------------
# Configuration
# -----------------------------------------------------------------------------
OUT_DIR = os.path.join(PROJECT_DIR, "bench", "results")
OUT_JSON = os.path.join(OUT_DIR, "benchmark4_fanout.json")
OUT_PNG = os.path.join(OUT_DIR, "benchmark4_fanout.png")

FANOUT_VALUES = [2, 3, 4, 5, 8, 10]
SUBSCRIBERS = 30
MEASURE_SECS = 20
WARMUP_SECS = 5
TIMEOUT_SECS = 120
PROBE_HZ = 50


# -----------------------------------------------------------------------------
# Benchmark Logic
# -----------------------------------------------------------------------------
def tree_depth(n_subscribers: int, fanout: int) -> int:
    """Calculate tree depth for N subscribers with given fanout."""
    if fanout <= 1 or n_subscribers <= 1:
        return 1
    return math.ceil(math.log(n_subscribers, fanout))


def run_single(docker_cli, fanout: int) -> dict:
    """Run one test case."""
    
    proxies = choose_proxies(SUBSCRIBERS, fanout)
    depth = tree_depth(SUBSCRIBERS, fanout)
    
    log(f"\n=== fanout={fanout} | depth={depth} | proxies={proxies} ===")
    
    os.environ["FANOUT_FACTOR"] = str(fanout)
    os.environ["BENCH_LATENCY"] = "1"
    os.environ["BENCH_LATENCY_HZ"] = str(PROBE_HZ)
    
    teardown(docker_cli, volumes=True)
    compose_up(docker_cli, listeners=SUBSCRIBERS, proxies=proxies, env=None)
    
    if not wait_for_listeners(SUBSCRIBERS, TIMEOUT_SECS):
        teardown(docker_cli, volumes=True)
        return {"success": False, "error": "timeout", "fanout": fanout}
    
    log(f"  warmup {WARMUP_SECS}s...")
    time.sleep(WARMUP_SECS)
    
    log(f"  measuring for {MEASURE_SECS}s...")
    time.sleep(MEASURE_SECS)
    
    samples_by_listener = parse_latency_samples_from_logs(docker_cli, only_service="listener")
    all_samples = [x for vs in samples_by_listener.values() for x in vs]
    
    if all_samples:
        mean, std = mean_std(all_samples)
        p50 = percentile(all_samples, 50)
        p95 = percentile(all_samples, 95)
    else:
        mean = std = p50 = p95 = 0.0
    
    log(f"  p50={p50:.1f}ms p95={p95:.1f}ms (n={len(all_samples)})")
    
    teardown(docker_cli, volumes=True)
    
    return {
        "success": True,
        "fanout": fanout,
        "subscribers": SUBSCRIBERS,
        "proxies": proxies,
        "tree_depth": depth,
        "samples": len(all_samples),
        "mean_ms": mean,
        "std_ms": std,
        "p50_ms": p50,
        "p95_ms": p95,
    }


def run_benchmark():
    docker_cli = docker()
    results = {"runs": [], "config": {
        "fanout_values": FANOUT_VALUES,
        "subscribers": SUBSCRIBERS,
        "measure_secs": MEASURE_SECS,
    }}
    
    for fanout in FANOUT_VALUES:
        results["runs"].append(run_single(docker_cli, fanout))
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
    
    runs = {r["fanout"]: r for r in results["runs"] if r.get("success")}
    fanouts = sorted(runs.keys())
    if not fanouts:
        return
    
    fig, ax = plt.subplots()
    
    p50 = [runs[f]["p50_ms"] for f in fanouts]
    p95 = [runs[f]["p95_ms"] for f in fanouts]
    
    ax.plot(fanouts, p50, "o-", color=colors["hierarchical"], 
            linewidth=PLOT_STYLE["line_width"], markersize=PLOT_STYLE["marker_size"], label="p50")
    ax.plot(fanouts, p95, "s--", color=colors["direct"], 
            linewidth=PLOT_STYLE["line_width"], markersize=PLOT_STYLE["marker_size"], label="p95")
    
    ax.set_xlabel("Fanout Factor")
    ax.set_ylabel("Latency (ms)")
    ax.set_title(f"Latency vs Fanout ({SUBSCRIBERS} subscribers)")
    ax.legend()
    ax.set_xticks(fanouts)
    ax.set_ylim(bottom=0)
    
    fig.tight_layout()
    fig.savefig(OUT_PNG)
    plt.close(fig)
    log(f"wrote: {OUT_PNG}")


def validate(results):
    if not results.get("runs"):
        return False
    need = set(FANOUT_VALUES)
    have = {r["fanout"] for r in results["runs"] 
            if r.get("success") and r.get("samples", 0) > 0}
    return need == have


def main():
    log("=" * 60)
    log("BENCHMARK 4: FANOUT TUNING")
    log("Question: What fanout gives the best latency-cost tradeoff?")
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

