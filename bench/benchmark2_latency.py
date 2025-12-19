#!/usr/bin/env python3
"""
Benchmark 2: Latency
====================

RESEARCH QUESTION:
    What is the latency cost of hierarchical routing?

HYPOTHESIS:
    - Direct: Lower latency (single hop from publisher to subscriber)
    - Hierarchical: Higher latency (multiple hops through proxy tree)
    - Latency increases with tree depth (log_fanout(N) hops)

WHAT WE MEASURE:
    - End-to-end message latency (time from publish to receive)
    - Reported as p50, p95, p99 percentiles
    - Jitter = p95 - p50 (latency variance)

WHY THIS MATTERS:
    Control loops have latency budgets. Understanding the latency cost of 
    hierarchical routing helps operators choose the right configuration.

EXPECTED RESULTS:
    - Direct: ~190ms baseline latency (WebRTC overhead), stable with N
    - Hierarchical: Higher latency, increasing slightly with N as tree deepens
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
OUT_JSON = os.path.join(OUT_DIR, "benchmark2_latency.json")
OUT_PNG = os.path.join(OUT_DIR, "benchmark2_latency.png")

SUBSCRIBER_COUNTS = [1, 5, 10, 20, 30, 40, 50]
FANOUT = 3
MEASURE_SECS = 30         # Longer for more latency samples
WARMUP_SECS = 5
TIMEOUT_SECS = 180
PROBE_HZ = 50             # Latency probe frequency


# -----------------------------------------------------------------------------
# Benchmark Logic
# -----------------------------------------------------------------------------
def run_single(docker_cli, mode: str, n_subscribers: int) -> dict:
    """Run one test case."""
    
    if mode == "direct":
        fanout, proxies = 1000, 0
    else:
        fanout = FANOUT
        proxies = choose_proxies(n_subscribers, fanout)
    
    log(f"\n=== {mode} | subscribers={n_subscribers} ===")
    
    os.environ["FANOUT_FACTOR"] = str(fanout)
    os.environ["BENCH_LATENCY"] = "1"
    os.environ["BENCH_LATENCY_HZ"] = str(PROBE_HZ)
    
    teardown(docker_cli, volumes=True)
    compose_up(docker_cli, listeners=n_subscribers, proxies=proxies, env=None)
    
    if not wait_for_listeners(n_subscribers, TIMEOUT_SECS):
        teardown(docker_cli, volumes=True)
        return {"success": False, "error": "timeout"}
    
    log(f"  warmup {WARMUP_SECS}s...")
    time.sleep(WARMUP_SECS)
    
    log(f"  collecting latency samples for {MEASURE_SECS}s...")
    time.sleep(MEASURE_SECS)
    
    # Parse latency samples from container logs
    samples_by_listener = parse_latency_samples_from_logs(docker_cli, only_service="listener")
    all_samples = [x for vs in samples_by_listener.values() for x in vs]
    
    if all_samples:
        mean, std = mean_std(all_samples)
        p50 = percentile(all_samples, 50)
        p95 = percentile(all_samples, 95)
        p99 = percentile(all_samples, 99)
        jitter = p95 - p50  # Tail spread as jitter metric
    else:
        mean = std = p50 = p95 = p99 = jitter = 0.0
    
    log(f"  p50={p50:.1f}ms p95={p95:.1f}ms jitter={jitter:.1f}ms (n={len(all_samples)})")
    
    teardown(docker_cli, volumes=True)
    
    return {
        "success": True,
        "mode": mode,
        "subscribers": n_subscribers,
        "samples": len(all_samples),
        "mean_ms": mean,
        "std_ms": std,
        "p50_ms": p50,
        "p95_ms": p95,
        "p99_ms": p99,
        "jitter_ms": jitter,
    }


def run_benchmark():
    docker_cli = docker()
    results = {"runs": [], "config": {
        "subscriber_counts": SUBSCRIBER_COUNTS,
        "fanout": FANOUT,
        "measure_secs": MEASURE_SECS,
        "probe_hz": PROBE_HZ,
    }}
    
    for n in SUBSCRIBER_COUNTS:
        results["runs"].append(run_single(docker_cli, "hierarchical", n))
        results["runs"].append(run_single(docker_cli, "direct", n))
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
        return
    
    # Single plot: Latency with error bars
    fig, ax = plt.subplots()
    
    direct_p50 = [direct[n]["p50_ms"] for n in xs]
    direct_p95 = [direct[n]["p95_ms"] for n in xs]
    hier_p50 = [hier[n]["p50_ms"] for n in xs]
    hier_p95 = [hier[n]["p95_ms"] for n in xs]
    
    x_pos = list(range(len(xs)))
    
    ax.bar([p - width/2 for p in x_pos], direct_p50, width, 
           yerr=[[0]*len(xs), [p95-p50 for p50, p95 in zip(direct_p50, direct_p95)]],
           label="Direct", color=colors["direct"], capsize=3, error_kw={"lw": 1.5})
    ax.bar([p + width/2 for p in x_pos], hier_p50, width,
           yerr=[[0]*len(xs), [p95-p50 for p50, p95 in zip(hier_p50, hier_p95)]],
           label="Hierarchical", color=colors["hierarchical"], capsize=3, error_kw={"lw": 1.5})
    
    ax.set_xlabel("Number of Subscribers")
    ax.set_ylabel("Latency (ms)")
    ax.set_title("End-to-End Transmission Latency (p50, whisker to p95)")
    ax.set_xticks(x_pos)
    ax.set_xticklabels([str(x) for x in xs])
    ax.legend()
    ax.set_ylim(bottom=0)
    
    fig.tight_layout()
    fig.savefig(OUT_PNG)
    plt.close(fig)
    log(f"wrote: {OUT_PNG}")


def validate(results):
    if not results.get("runs"):
        return False
    need = {(n, m) for n in SUBSCRIBER_COUNTS for m in ["hierarchical", "direct"]}
    have = {(r["subscribers"], r["mode"]) for r in results["runs"] 
            if r.get("success") and r.get("samples", 0) > 0}
    return need == have


def main():
    log("=" * 60)
    log("BENCHMARK 2: LATENCY")
    log("Question: What is the latency cost of hierarchical routing?")
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

