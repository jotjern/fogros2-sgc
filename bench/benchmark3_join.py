#!/usr/bin/env python3
"""
Benchmark 3: Join Latency
=========================

RESEARCH QUESTION:
    How long does it take for a new subscriber to receive its first message?

HYPOTHESIS (from paper):
    - At publish rate f=10Hz, ideal join latency = 1/(2f) = 50ms average
    - Direct routing: ~1 connection to establish, so close to ideal + t_connection
    - Hierarchical: Multiple connections (tree path), so max(t_0, t_1, ..., t_n)
    - Hierarchical should be slower due to tree construction overhead

WHAT WE MEASURE:
    - Time from subscription initiation to first message received
    - This is instrumented in Rust code and stored in Redis:
      - bench_join_attempt_ms: when subscribe() is called
      - bench_first_msg_ms: when first payload arrives
    - Reported as p50, p95 to show distribution

WHY THIS MATTERS:
    Dynamic robot fleets need fast join times. Understanding the overhead
    of hierarchical routing helps operators decide when it's appropriate.

EXPECTED RESULTS:
    - Direct: Close to ideal (~50-200ms depending on connection time)
    - Hierarchical: Higher p95 due to tree path construction
"""
import json
import os
import time

from bench_utils import (
    PROJECT_DIR,
    choose_proxies,
    compose_up,
    docker,
    join_latency_from_redis,
    listener_hostnames,
    log,
    mean_std,
    percentile,
    teardown,
    wait_for_listeners,
)

# -----------------------------------------------------------------------------
# Configuration
# -----------------------------------------------------------------------------
OUT_DIR = os.path.join(PROJECT_DIR, "bench", "results")
OUT_JSON = os.path.join(OUT_DIR, "benchmark3_join.json")
OUT_PNG = os.path.join(OUT_DIR, "benchmark3_join.png")

_DEFAULT_COUNTS = [1, 5, 10, 15, 20, 25, 30]
_only_n = os.environ.get("BENCH3_ONLY_N")
if _only_n:
    SUBSCRIBER_COUNTS = [int(_only_n)]
else:
    SUBSCRIBER_COUNTS = list(_DEFAULT_COUNTS)
FANOUT = 3
PUBLISH_HZ = 10.0          # Message rate (for ideal calculation)
TIMEOUT_SECS = 180
JOIN_WAIT_SECS = 60.0      # Max time to collect join data
TRIALS = int(os.environ.get("BENCH3_TRIALS", "1"))
ONLY_MODE = os.environ.get("BENCH3_ONLY_MODE")  # "hierarchical" or "direct"
DUMP_LOGS = os.environ.get("BENCH3_DUMP_LOGS", "0") in ("1", "true", "TRUE", "yes", "YES")


# -----------------------------------------------------------------------------
# Benchmark Logic
# -----------------------------------------------------------------------------
def run_single(docker_cli, mode: str, n_subscribers: int) -> dict:
    """Run one test case (optionally repeated TRIALS times).

    We pool per-listener join measurements across trials to get more stable
    tail statistics (p95 in particular) for hierarchical routing.
    """
    
    if mode == "direct":
        fanout, proxies = 1000, 0
    else:
        fanout = FANOUT
        proxies = choose_proxies(n_subscribers, fanout)
    
    log(f"\n=== {mode} | subscribers={n_subscribers} | trials={TRIALS} ===")
    
    os.environ["FANOUT_FACTOR"] = str(fanout)
    os.environ["BENCH_LATENCY"] = "1"
    os.environ["BENCH_LATENCY_HZ"] = str(PUBLISH_HZ)
    
    all_vals = []
    per_trial = []
    success_trials = 0
    for trial in range(TRIALS):
        log(f"  trial {trial+1}/{TRIALS}...")
        teardown(docker_cli, volumes=True)
        compose_up(docker_cli, listeners=n_subscribers, proxies=proxies, env=None)

        if not wait_for_listeners(n_subscribers, TIMEOUT_SECS):
            teardown(docker_cli, volumes=True)
            per_trial.append({"trial": trial + 1, "success": False, "error": "timeout"})
            continue

        # Collect join latency from Redis
        log(f"    collecting join latency (up to {JOIN_WAIT_SECS}s)...")
        hostnames = listener_hostnames(docker_cli)
        join_ms = join_latency_from_redis(hostnames, JOIN_WAIT_SECS)

        vals = list(join_ms.values())
        missing = len(hostnames) - len(vals)
        all_vals.extend(vals)
        success_trials += 1
        per_trial.append({
            "trial": trial + 1,
            "success": True,
            "collected": len(vals),
            "missing": missing,
        })
        log(f"    collected={len(vals)} missing={missing}")

        if DUMP_LOGS:
            log("    dumping container logs...")
            log_dir = os.path.join(OUT_DIR, "benchmark3_logs", f"{mode}_n{n_subscribers}_trial{trial+1}")
            os.makedirs(log_dir, exist_ok=True)
            ps = docker_cli.compose.ps()
            log(f"    containers: {len(ps)}")
            for c in ps:
                name = getattr(c, "name", None) or getattr(c, "container_name", None) or c.id
                safe = str(name).replace("/", "_")
                try:
                    txt = docker_cli.container.logs(c.id)
                except Exception as e:
                    txt = f"(error fetching logs: {e})\n"
                if isinstance(txt, bytes):
                    txt = txt.decode("utf-8", errors="replace")
                with open(os.path.join(log_dir, f"{safe}.log"), "w", encoding="utf-8") as f:
                    f.write(txt or "")
            log(f"    logs saved to {log_dir}/")

        teardown(docker_cli, volumes=True)

    if all_vals:
        mean, std = mean_std(all_vals)
        p50 = percentile(all_vals, 50)
        p95 = percentile(all_vals, 95)
    else:
        mean = std = p50 = p95 = 0.0
    
    return {
        "success": success_trials > 0,
        "mode": mode,
        "subscribers": n_subscribers,
        "trials": TRIALS,
        "success_trials": success_trials,
        "collected": len(all_vals),
        "expected": int(n_subscribers) * int(TRIALS),
        "missing": (int(n_subscribers) * int(TRIALS)) - len(all_vals),
        "mean_ms": mean,
        "std_ms": std,
        "p50_ms": p50,
        "p95_ms": p95,
        "per_trial": per_trial,
    }


def run_benchmark():
    docker_cli = docker()
    results = {"runs": [], "config": {
        "subscriber_counts": SUBSCRIBER_COUNTS,
        "fanout": FANOUT,
        "publish_hz": PUBLISH_HZ,
        "ideal_avg_ms": 1000 / (2 * PUBLISH_HZ),  # Theoretical minimum
        "trials": TRIALS,
        "only_mode": ONLY_MODE,
    }}
    
    for n in SUBSCRIBER_COUNTS:
        if ONLY_MODE in (None, "", "hierarchical"):
            results["runs"].append(run_single(docker_cli, "hierarchical", n))
        if ONLY_MODE in (None, "", "direct"):
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
    
    fig, ax = plt.subplots()
    
    x_pos = list(range(len(xs)))
    
    direct_p50 = [direct[n]["p50_ms"] for n in xs]
    direct_p95 = [direct[n]["p95_ms"] for n in xs]
    hier_p50 = [hier[n]["p50_ms"] for n in xs]
    hier_p95 = [hier[n]["p95_ms"] for n in xs]
    
    direct_err = [[0]*len(xs), [p95-p50 for p50, p95 in zip(direct_p50, direct_p95)]]
    hier_err = [[0]*len(xs), [p95-p50 for p50, p95 in zip(hier_p50, hier_p95)]]
    
    ax.bar([p - width/2 for p in x_pos], direct_p50, width,
           yerr=direct_err, capsize=3, error_kw={"lw": 1.5},
           label="Direct", color=colors["direct"])
    ax.bar([p + width/2 for p in x_pos], hier_p50, width,
           yerr=hier_err, capsize=3, error_kw={"lw": 1.5},
           label="Hierarchical", color=colors["hierarchical"])
    
    # Ideal line
    ideal_ms = results.get("config", {}).get("ideal_avg_ms", 50)
    ax.axhline(ideal_ms, color=colors["neutral"], linestyle="--", linewidth=1.5,
               label=f"Theoretical minimum ({ideal_ms:.0f}ms)")
    
    ax.set_xlabel("Number of Subscribers")
    ax.set_ylabel("Join Latency (ms)")
    ax.set_title("Time to First Message (p50, whisker to p95)")
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
    modes = ["hierarchical", "direct"]
    if ONLY_MODE in ("hierarchical", "direct"):
        modes = [ONLY_MODE]
    need = {(n, m) for n in SUBSCRIBER_COUNTS for m in modes}
    have = {(r["subscribers"], r["mode"]) for r in results["runs"] 
            if r.get("success") and r.get("collected", 0) > 0}
    return need == have


def main():
    log("=" * 60)
    log("BENCHMARK 3: JOIN LATENCY")
    log("Question: How long until a new subscriber receives its first message?")
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

