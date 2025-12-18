#!/usr/bin/env python3
import json
import os
from bench_utils import (
    PROJECT_DIR,
    choose_proxies,
    compose_up,
    docker,
    join_latency_from_redis,
    listener_hostnames,
    log,
    teardown,
    wait_for_listeners,
)

OUT_DIR = os.path.join(PROJECT_DIR, "bench", "results")
OUT_JSON = os.path.join(OUT_DIR, "benchmark2_join_latency.json")
OUT_PNG = os.path.join(OUT_DIR, "benchmark2_join_latency.png")

# What we want (hardcoded):
LISTENER_SET = [1, 5, 10, 15, 25]
FANOUT_HIER = 3
FANOUT_DIRECT = 1000
TIMEOUT_SECS = 240
LATENCY_HZ = 10.0
JOIN_WAIT_SECS = 60.0


def run_case(docker_cli, *, mode, fanout, listeners, timeout_secs, latency_hz, join_wait_secs):
    if mode == "direct":
        proxy_count = 0
    else:
        proxy_count = choose_proxies(listeners, fanout)

    log(f"\n=== benchmark2: {mode} fanout={fanout} listeners={listeners} proxies={proxy_count} ===")

    os.environ["FANOUT_FACTOR"] = str(fanout)
    os.environ["BENCH_LATENCY"] = "1"
    os.environ["BENCH_LATENCY_HZ"] = str(latency_hz)

    log("  bringing up docker compose...")
    teardown(docker_cli, volumes=True)
    compose_up(docker_cli, listeners=listeners, proxies=proxy_count, env=None)

    log("  waiting for all listeners to connect...")
    ok = wait_for_listeners(listeners, timeout_secs)
    if not ok:
        log("  ERROR: listeners did not connect in time")
        teardown(docker_cli, volumes=True)
        return {"success": False, "error": "listeners did not connect in time"}
    log("  all listeners connected")

    log(f"  measuring join latency from routing subscribe -> first received message (waiting up to {join_wait_secs}s)...")
    hostnames = listener_hostnames(docker_cli)
    join_ms = join_latency_from_redis(hostnames, join_wait_secs)
    vals = list(join_ms.values())
    avg = (sum(vals) / len(vals)) if vals else None
    log(f"  join done: got={len(vals)} missing={max(0, len(hostnames) - len(vals))}")

    teardown(docker_cli, volumes=True)

    return {
        "success": True,
        "fanout": fanout,
        "mode": mode,
        "listeners": listeners,
        "proxies": proxy_count,
        "latency_hz": latency_hz,
        "join_wait_secs": join_wait_secs,
        "join": {
            "avg_secs": (avg / 1000.0) if avg is not None else None,
            "count": len(vals),
            "missing": max(0, len(hostnames) - len(vals)),
        },
    }


def plot_bar(xs, direct_vals, hier_vals, out_path, ideal_ms=50.0):
    import matplotlib.pyplot as plt

    fig, ax = plt.subplots(figsize=(10.5, 4.8))
    x_pos = list(range(len(xs)))
    width = 0.38
    ax.bar([p - width / 2 for p in x_pos], direct_vals, width=width, label="direct (fanout=1000, proxies=0)")
    ax.bar([p + width / 2 for p in x_pos], hier_vals, width=width, label="hierarchical (fanout=3)")
    ax.axhline(ideal_ms, color="black", linestyle="--", linewidth=1.2, label="ideal (50ms @ 10Hz)")
    ax.set_title("Join latency vs listeners (avg time to first message)")
    ax.set_xlabel("Listeners")
    ax.set_ylabel("Join latency (ms)")
    ax.set_xticks(x_pos)
    ax.set_xticklabels([str(x) for x in xs])
    ax.grid(True, axis="y", linestyle="--", linewidth=0.6, alpha=0.6)
    ax.legend()
    os.makedirs(os.path.dirname(out_path) or ".", exist_ok=True)
    fig.tight_layout()
    fig.savefig(out_path, dpi=160)


def _extract_or_raise(payload):
    if not isinstance(payload, dict):
        raise ValueError("payload must be a dict")
    runs = payload.get("runs")
    if not isinstance(runs, list):
        raise ValueError("payload must have a 'runs' list")

    need = {(n, "hierarchical") for n in LISTENER_SET} | {(n, "direct") for n in LISTENER_SET}
    out = {}
    for r in runs:
        if not isinstance(r, dict):
            continue
        if r.get("success") is not True:
            raise ValueError(f"found failed run: {r}")
        key = (r.get("listeners"), r.get("mode"))
        if key not in need:
            continue
        j = r.get("join") or {}
        avg = j.get("avg_secs")
        if avg is None:
            raise ValueError(f"run missing join.avg_secs: {r}")
        if int(j.get("count", 0) or 0) <= 0:
            raise ValueError(f"run has no join samples: {r}")
        out[key] = float(avg)

    missing = [k for k in sorted(need) if k not in out]
    if missing:
        raise ValueError(f"missing runs: {missing}")
    return out


def _plot(extracted):
    xs = list(LISTENER_SET)
    d_vals = [extracted[(n, "direct")] * 1000.0 for n in xs]
    h_vals = [extracted[(n, "hierarchical")] * 1000.0 for n in xs]
    plot_bar(xs, d_vals, h_vals, OUT_PNG, ideal_ms=50.0)


def main():
    xs = list(LISTENER_SET)
    log("=== benchmark2: join latency sweep ===")
    log(f"  listeners={xs}")
    log(f"  latency_hz={LATENCY_HZ} join_wait_secs={JOIN_WAIT_SECS} timeout_secs={TIMEOUT_SECS}")
    log(f"  hierarchical fanout={FANOUT_HIER} | direct fanout={FANOUT_DIRECT} (proxies=0)")

    if os.path.exists(OUT_JSON):
        try:
            with open(OUT_JSON, "r") as f:
                cached = json.load(f)
            extracted = _extract_or_raise(cached)
            log(f"  using cached data: {OUT_JSON}")
            _plot(extracted)
            log(f"wrote: {OUT_PNG}")
            return
        except Exception as e:
            log(f"  cached data invalid, re-running benchmark: {e}")

    docker_cli = docker()
    results = {"runs": []}
    os.makedirs(OUT_DIR, exist_ok=True)

    for n in xs:
        results["runs"].append(
            run_case(
                docker_cli,
                mode="hierarchical",
                fanout=FANOUT_HIER,
                listeners=n,
                timeout_secs=TIMEOUT_SECS,
                latency_hz=LATENCY_HZ,
                join_wait_secs=JOIN_WAIT_SECS,
            )
        )
        results["runs"].append(
            run_case(
                docker_cli,
                mode="direct",
                fanout=FANOUT_DIRECT,
                listeners=n,
                timeout_secs=TIMEOUT_SECS,
                latency_hz=LATENCY_HZ,
                join_wait_secs=JOIN_WAIT_SECS,
            )
        )

    with open(OUT_JSON, "w") as f:
        json.dump(results, f, indent=2)

    extracted = _extract_or_raise(results)
    _plot(extracted)

    log(f"wrote: {OUT_JSON}")
    log(f"wrote: {OUT_PNG}")


if __name__ == "__main__":
    main()

