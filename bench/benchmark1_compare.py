#!/usr/bin/env python3
import json
import os

from bench_utils import (
    PROJECT_DIR,
    choose_proxies,
    compose_up,
    docker,
    log,
    measure_net,
    parse_latency_samples_from_logs,
    teardown,
    wait_for_listeners,
)


OUT_DIR = os.path.join(PROJECT_DIR, "bench", "results")
OUT_JSON = os.path.join(OUT_DIR, "benchmark1_compare.json")
OUT_THR = os.path.join(OUT_DIR, "benchmark1_throughput.png")
OUT_LAT = os.path.join(OUT_DIR, "benchmark1_latency.png")

# What we want (hardcoded):
LISTENER_SET = [1, 5, 10, 15, 25]
FANOUT_HIER = 3
FANOUT_DIRECT = 1000
MEASURE_SECS = 30
TIMEOUT_SECS = 240
LATENCY_HZ = 20.0


def run_case(docker_cli, *, mode, fanout, listeners, measure_secs, timeout_secs, latency_hz):
    if mode == "direct":
        proxy_count = 0
    else:
        proxy_count = choose_proxies(listeners, fanout)

    log(f"\n=== benchmark1: {mode} fanout={fanout} listeners={listeners} proxies={proxy_count} ===")

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

    log(f"  measuring throughput for {measure_secs}s...")
    _d, talker_tx_mbps, listener_rx_mbps = measure_net(docker_cli, measure_secs)

    log("  parsing listener latency samples from logs...")
    lat = parse_latency_samples_from_logs(docker_cli, only_service="listener")
    per_listener_mean = {k: (sum(v) / len(v) if v else 0.0) for k, v in lat.items()}
    means = list(per_listener_mean.values())
    avg_latency_ms = (sum(means) / len(means)) if means else 0.0

    log("  tearing down docker compose...")
    teardown(docker_cli, volumes=True)

    return {
        "success": True,
        "fanout": fanout,
        "mode": mode,
        "listeners": listeners,
        "proxies": proxy_count,
        "measure_secs": measure_secs,
        "throughput": {
            "talker_tx_mbps": talker_tx_mbps,
            "listener_rx_mbps": listener_rx_mbps,
        },
        "latency": {
            "avg_per_listener_mean_ms": avg_latency_ms,
        },
    }


def plot_bar(xs, a_vals, b_vals, a_label, b_label, title, ylabel, out_path):
    import matplotlib.pyplot as plt

    fig, ax = plt.subplots(figsize=(10.5, 4.8))
    x_pos = list(range(len(xs)))
    width = 0.38
    ax.bar([p - width / 2 for p in x_pos], a_vals, width=width, label=a_label)
    ax.bar([p + width / 2 for p in x_pos], b_vals, width=width, label=b_label)
    ax.set_title(title)
    ax.set_xlabel("Listeners")
    ax.set_ylabel(ylabel)
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
        thr = ((r.get("throughput") or {}).get("talker_tx_mbps"))
        lat = ((r.get("latency") or {}).get("avg_per_listener_mean_ms"))
        if thr is None or lat is None:
            raise ValueError(f"run missing throughput/latency: {r}")
        out[key] = {"thr": float(thr), "lat": float(lat)}

    missing = [k for k in sorted(need) if k not in out]
    if missing:
        raise ValueError(f"missing runs: {missing}")
    return out


def _plot(extracted):
    xs = list(LISTENER_SET)
    thr_d = [extracted[(n, "direct")]["thr"] for n in xs]
    thr_h = [extracted[(n, "hierarchical")]["thr"] for n in xs]
    lat_d = [extracted[(n, "direct")]["lat"] for n in xs]
    lat_h = [extracted[(n, "hierarchical")]["lat"] for n in xs]

    plot_bar(
        xs,
        thr_d,
        thr_h,
        "direct (fanout=1000, proxies=0)",
        "hierarchical (fanout=3)",
        "Throughput vs listeners",
        "Publisher TX (Mbps)",
        OUT_THR,
    )
    plot_bar(
        xs,
        lat_d,
        lat_h,
        "direct (fanout=1000, proxies=0)",
        "hierarchical (fanout=3)",
        "Latency vs listeners",
        "Avg latency per listener (ms)",
        OUT_LAT,
    )


def main():
    xs = list(LISTENER_SET)
    log("=== benchmark1: hierarchical vs direct sweep ===")
    log(f"  listeners={xs}")
    log(f"  measure_secs={MEASURE_SECS} timeout_secs={TIMEOUT_SECS} latency_hz={LATENCY_HZ}")
    log(f"  hierarchical fanout={FANOUT_HIER} | direct fanout={FANOUT_DIRECT} (proxies=0)")

    if os.path.exists(OUT_JSON):
        try:
            with open(OUT_JSON, "r") as f:
                cached = json.load(f)
            extracted = _extract_or_raise(cached)
            log(f"  using cached data: {OUT_JSON}")
            _plot(extracted)
            log(f"wrote: {OUT_THR}")
            log(f"wrote: {OUT_LAT}")
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
                measure_secs=MEASURE_SECS,
                timeout_secs=TIMEOUT_SECS,
                latency_hz=LATENCY_HZ,
            )
        )
        results["runs"].append(
            run_case(
                docker_cli,
                mode="direct",
                fanout=FANOUT_DIRECT,
                listeners=n,
                measure_secs=MEASURE_SECS,
                timeout_secs=TIMEOUT_SECS,
                latency_hz=LATENCY_HZ,
            )
        )

    with open(OUT_JSON, "w") as f:
        json.dump(results, f, indent=2)

    extracted = _extract_or_raise(results)
    _plot(extracted)

    log(f"wrote: {OUT_JSON}")
    log(f"wrote: {OUT_THR}")
    log(f"wrote: {OUT_LAT}")


if __name__ == "__main__":
    main()

