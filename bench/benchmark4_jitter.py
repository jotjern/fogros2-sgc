#!/usr/bin/env python3
import json
import math
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
OUT_JSON = os.path.join(OUT_DIR, "benchmark4_jitter.json")
OUT_STD = os.path.join(OUT_DIR, "benchmark4_jitter_std.png")
OUT_P95 = os.path.join(OUT_DIR, "benchmark4_jitter_p95p50.png")

# What we want (hardcoded):
LISTENER_SET = [1, 5, 10, 15, 25]
FANOUT_HIER = 3
FANOUT_DIRECT = 1000
MEASURE_SECS = 5
TIMEOUT_SECS = 240
LATENCY_HZ = 20.0


def mean_std(vals):
    if not vals:
        return 0.0, 0.0
    m = sum(vals) / len(vals)
    v = sum((x - m) ** 2 for x in vals) / len(vals)
    return m, math.sqrt(max(0.0, v))


def pctl(vals, p):
    if not vals:
        return 0.0
    s = sorted(vals)
    k = int(round((p / 100.0) * (len(s) - 1)))
    k = max(0, min(len(s) - 1, k))
    return float(s[k])


def run_case(docker_cli, *, mode, fanout, listeners, measure_secs, timeout_secs, latency_hz):
    if mode == "direct":
        proxy_count = 0
    else:
        proxy_count = choose_proxies(listeners, fanout)

    log(f"\n=== benchmark4: {mode} fanout={fanout} listeners={listeners} proxies={proxy_count} ===")

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

    log(f"  measuring for {measure_secs}s...")
    _d, talker_tx_mbps, _listener_rx_mbps = measure_net(docker_cli, measure_secs)

    log("  parsing latency samples and computing jitter...")
    lat = parse_latency_samples_from_logs(docker_cli, only_service="listener")
    per_listener_mean = {}
    per_listener_std = {}
    for k, v in lat.items():
        m, s = mean_std(v)
        per_listener_mean[k] = m
        per_listener_std[k] = s

    stds = [s for s in per_listener_std.values() if s > 0]
    jitter_std_avg_ms = (sum(stds) / len(stds)) if stds else 0.0

    all_samples = [x for vs in lat.values() for x in vs]
    jitter_p95_p50_ms = pctl(all_samples, 95) - pctl(all_samples, 50)

    teardown(docker_cli, volumes=True)

    return {
        "success": True,
        "fanout": fanout,
        "mode": mode,
        "listeners": listeners,
        "proxies": proxy_count,
        "measure_secs": measure_secs,
        "throughput": {"talker_tx_mbps": talker_tx_mbps},
        "jitter": {
            "std_avg_ms": jitter_std_avg_ms,
            "p95_p50_ms": jitter_p95_p50_ms,
        },
    }


def plot_bar(xs, direct_vals, hier_vals, title, ylabel, out_path):
    import matplotlib.pyplot as plt

    fig, ax = plt.subplots(figsize=(10.5, 4.8))
    x_pos = list(range(len(xs)))
    width = 0.38
    ax.bar([p - width / 2 for p in x_pos], direct_vals, width=width, label="direct (fanout=1000, proxies=0)")
    ax.bar([p + width / 2 for p in x_pos], hier_vals, width=width, label="hierarchical (fanout=3)")
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
        j = r.get("jitter") or {}
        if "std_avg_ms" not in j or "p95_p50_ms" not in j:
            raise ValueError(f"run missing jitter fields: {r}")
        out[key] = {"std": float(j["std_avg_ms"]), "p95p50": float(j["p95_p50_ms"])}

    missing = [k for k in sorted(need) if k not in out]
    if missing:
        raise ValueError(f"missing runs: {missing}")
    return out


def _plot(extracted):
    xs = list(LISTENER_SET)
    d_std = [extracted[(n, "direct")]["std"] for n in xs]
    h_std = [extracted[(n, "hierarchical")]["std"] for n in xs]
    d_p95 = [extracted[(n, "direct")]["p95p50"] for n in xs]
    h_p95 = [extracted[(n, "hierarchical")]["p95p50"] for n in xs]
    plot_bar(xs, d_std, h_std, "Jitter vs listeners (stddev of latency)", "Stddev (ms)", OUT_STD)
    plot_bar(xs, d_p95, h_p95, "Jitter vs listeners (p95 - p50)", "p95 - p50 (ms)", OUT_P95)


def main():
    xs = list(LISTENER_SET)
    log("=== benchmark4: jitter sweep ===")
    log(f"  listeners={xs}")
    log(f"  measure_secs={MEASURE_SECS} timeout_secs={TIMEOUT_SECS} latency_hz={LATENCY_HZ}")

    if os.path.exists(OUT_JSON):
        try:
            with open(OUT_JSON, "r") as f:
                cached = json.load(f)
            extracted = _extract_or_raise(cached)
            log(f"  using cached data: {OUT_JSON}")
            _plot(extracted)
            log(f"wrote: {OUT_STD}")
            log(f"wrote: {OUT_P95}")
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
    log(f"wrote: {OUT_STD}")
    log(f"wrote: {OUT_P95}")


if __name__ == "__main__":
    main()

