#!/usr/bin/env python3
import json
import os
import time
from collections import defaultdict
from random import Random

from bench_utils import (
    PROJECT_DIR,
    choose_proxies,
    connected_listener_count,
    docker,
    hop_histogram,
    log,
    net_delta,
    net_snapshot,
    service_of,
    teardown,
)

OUT_DIR = os.path.join(PROJECT_DIR, "bench", "results")
OUT_JSON = os.path.join(OUT_DIR, "benchmark3_storm.json")
OUT_HOPS = os.path.join(OUT_DIR, "benchmark3_hops.png")
OUT_RX = os.path.join(OUT_DIR, "benchmark3_listener_rx.png")

# What we want (hardcoded):
MODE = "hierarchical"  # (storm plots rely on hops; direct mode is less interesting here)
LISTENERS = 50
FANOUT = 3
MEASURE_INTERVAL_SECS = 1.0
PRE_SECS = 5.0
DOWN_SECS = 10.0
POST_SECS = 30.0
STORM_PERCENT = 50.0
TIMEOUT_SECS = 240
SEED = 1


def wait_for_listeners(expected, timeout_secs):
    start = time.time()
    while time.time() - start < timeout_secs:
        n = connected_listener_count()
        print(f"  {n}/{expected} listeners connected ({int(time.time()-start)}s)", flush=True)
        if n >= expected:
            return True
        time.sleep(3)
    return False


def plot_hops(storm, out_path):
    import matplotlib.pyplot as plt

    phases = ["pre", "down", "post"]
    phase_hists = {p: defaultdict(float) for p in phases}
    phase_counts = {p: 0 for p in phases}

    for s in storm["samples"]:
        p = s["phase"]
        if p not in phase_hists:
            continue
        phase_counts[p] += 1
        for hop, cnt in (s.get("hops", {}) or {}).items():
            phase_hists[p][int(hop)] += float(cnt)

    # average over time
    for p in phases:
        c = phase_counts[p] or 1
        for hop in list(phase_hists[p].keys()):
            phase_hists[p][hop] = phase_hists[p][hop] / c

    hops = sorted({h for p in phases for h in phase_hists[p].keys()})
    if not hops:
        hops = [1]

    x_pos = list(range(len(hops)))
    width = 0.26
    fig, ax = plt.subplots(figsize=(9.5, 4.8))
    ax.bar([p - width for p in x_pos], [phase_hists["pre"].get(h, 0.0) for h in hops], width=width, label="pre")
    ax.bar([p for p in x_pos], [phase_hists["down"].get(h, 0.0) for h in hops], width=width, label="down")
    ax.bar([p + width for p in x_pos], [phase_hists["post"].get(h, 0.0) for h in hops], width=width, label="post")
    ax.set_title("Hop count distribution during storm (avg over time)")
    ax.set_xlabel("Hops")
    ax.set_ylabel("Avg listeners at hop")
    ax.set_xticks(x_pos)
    ax.set_xticklabels([str(h) for h in hops])
    ax.grid(True, axis="y", linestyle="--", linewidth=0.6, alpha=0.6)
    ax.legend()
    os.makedirs(os.path.dirname(out_path) or ".", exist_ok=True)
    fig.tight_layout()
    fig.savefig(out_path, dpi=160)


def plot_listener_rx(storm, out_path):
    import matplotlib.pyplot as plt

    t = [s["t"] for s in storm["samples"]]
    rx = [s["listener_rx_mbps"] for s in storm["samples"]]
    phases = [s["phase"] for s in storm["samples"]]

    fig, ax = plt.subplots(figsize=(10.5, 4.8))
    ax.plot(t, rx, linewidth=2.0, label="listener_rx_mbps")
    ax.set_title("Listener incoming throughput during storm")
    ax.set_xlabel("Time (s)")
    ax.set_ylabel("Aggregate listener RX (Mbps)")
    ax.grid(True, axis="y", linestyle="--", linewidth=0.6, alpha=0.6)

    # mark transitions
    down_t = None
    up_t = None
    for i in range(1, len(phases)):
        if phases[i - 1] == "pre" and phases[i] == "down":
            down_t = t[i]
        if phases[i - 1] == "down" and phases[i] == "post":
            up_t = t[i]
    if down_t is not None:
        ax.axvline(down_t, color="red", linestyle="--", linewidth=1.2, label="storm down")
    if up_t is not None:
        ax.axvline(up_t, color="green", linestyle="--", linewidth=1.2, label="storm up")
    ax.legend()

    os.makedirs(os.path.dirname(out_path) or ".", exist_ok=True)
    fig.tight_layout()
    fig.savefig(out_path, dpi=160)


def _extract_or_raise(payload):
    if not isinstance(payload, dict):
        raise ValueError("payload must be a dict")
    run = payload.get("run")
    if not isinstance(run, dict):
        raise ValueError("payload must have a 'run' dict")
    samples = run.get("samples")
    if not isinstance(samples, list) or not samples:
        raise ValueError("run.samples missing/empty")
    phases = {s.get("phase") for s in samples if isinstance(s, dict)}
    if "down" not in phases or "post" not in phases:
        raise ValueError(f"missing storm phases (need down+post), have={sorted(phases)}")
    for s in samples:
        if not isinstance(s, dict):
            continue
        if "t" not in s or "listener_rx_mbps" not in s or "hops" not in s:
            raise ValueError(f"bad sample: {s}")
    return run


def _plot(run):
    plot_hops(run, OUT_HOPS)
    plot_listener_rx(run, OUT_RX)


def main():
    docker_cli = docker()

    if os.path.exists(OUT_JSON):
        try:
            with open(OUT_JSON, "r") as f:
                cached = json.load(f)
            run = _extract_or_raise(cached)
            log(f"  using cached data: {OUT_JSON}")
            _plot(run)
            log(f"wrote: {OUT_HOPS}")
            log(f"wrote: {OUT_RX}")
            return
        except Exception as e:
            log(f"  cached data invalid, re-running benchmark: {e}")

    proxy_count = 0 if MODE == "direct" else choose_proxies(LISTENERS, FANOUT)
    log("=== benchmark3: storm ===")
    log(f"  mode={MODE} fanout={FANOUT} listeners={LISTENERS} proxies={proxy_count}")
    log(f"  storm_percent={STORM_PERCENT} pre={PRE_SECS}s down={DOWN_SECS}s post={POST_SECS}s sample_interval={MEASURE_INTERVAL_SECS}s")
    os.environ["FANOUT_FACTOR"] = str(FANOUT)
    os.environ["BENCH_LATENCY"] = "1"
    os.environ["BENCH_LATENCY_HZ"] = "10"

    log("  bringing up docker compose...")
    teardown(docker_cli, volumes=True)
    docker_cli.compose.up(detach=True, scales={"listener": LISTENERS, "proxy": proxy_count})

    log("  waiting for all listeners to connect...")
    ok = wait_for_listeners(LISTENERS, TIMEOUT_SECS)
    if not ok:
        teardown(docker_cli, volumes=True)
        raise SystemExit("listeners did not connect in time")
    log("  all listeners connected")

    rng = Random(int(SEED))
    cache = {}
    listener_containers = [
        (c.id, getattr(c, "container_name", "") or c.id)
        for c in docker_cli.compose.ps()
        if service_of(docker_cli, c, cache) == "listener"
    ]
    rng.shuffle(listener_containers)
    k = int(round((max(0.0, min(100.0, float(STORM_PERCENT))) / 100.0) * len(listener_containers)))
    if len(listener_containers) > 0 and float(STORM_PERCENT) > 0:
        k = max(1, k)
    k = min(k, len(listener_containers))
    down = listener_containers[:k]
    log(f"  selected {len(down)}/{len(listener_containers)} listeners to stop during storm")

    samples = []
    t0 = time.time()
    last_net = net_snapshot(docker_cli)

    def sample(phase):
        nonlocal last_net
        now = time.time()
        cur = net_snapshot(docker_cli)
        d = net_delta(last_net, cur)
        last_net = cur
        dt = max(0.001, float(MEASURE_INTERVAL_SECS))
        talker_tx = sum(int(v.get("tx", 0)) for v in d.values() if (v or {}).get("service") == "talker")
        listener_rx = sum(int(v.get("rx", 0)) for v in d.values() if (v or {}).get("service") == "listener")
        samples.append(
            {
                "t": now - t0,
                "phase": phase,
                "talker_tx_mbps": (talker_tx * 8.0) / (dt * 1e6),
                "listener_rx_mbps": (listener_rx * 8.0) / (dt * 1e6),
                "connected_listeners": connected_listener_count(),
                "hops": hop_histogram(),
            }
        )

    end_pre = time.time() + float(PRE_SECS)
    log("  phase=pre sampling...")
    while time.time() < end_pre:
        sample("pre")
        time.sleep(float(MEASURE_INTERVAL_SECS))

    log("  phase=down stopping listeners...")
    for cid, _ in down:
        try:
            docker_cli.container.stop(cid)
        except Exception:
            pass

    end_down = time.time() + float(DOWN_SECS)
    log("  phase=down sampling...")
    while time.time() < end_down:
        sample("down")
        time.sleep(float(MEASURE_INTERVAL_SECS))

    log("  phase=post starting listeners...")
    for cid, _ in down:
        try:
            docker_cli.container.start(cid)
        except Exception:
            pass

    end_post = time.time() + float(POST_SECS)
    log("  phase=post sampling...")
    while time.time() < end_post:
        sample("post")
        time.sleep(float(MEASURE_INTERVAL_SECS))

    log("  tearing down docker compose...")
    teardown(docker_cli, volumes=True)

    payload = {
        "meta": {
            "mode": MODE,
            "fanout": FANOUT,
            "listeners": LISTENERS,
            "measure_interval_secs": MEASURE_INTERVAL_SECS,
            "pre_secs": PRE_SECS,
            "down_secs": DOWN_SECS,
            "post_secs": POST_SECS,
            "storm_percent": STORM_PERCENT,
            "timeout_secs": TIMEOUT_SECS,
            "seed": SEED,
        },
        "run": {
            "mode": MODE,
            "fanout": FANOUT,
            "listeners": LISTENERS,
            "proxies": proxy_count,
            "storm_percent": float(STORM_PERCENT),
            "down_count": len(down),
            "samples": samples,
        },
    }
    os.makedirs(OUT_DIR, exist_ok=True)
    with open(OUT_JSON, "w") as f:
        json.dump(payload, f, indent=2)

    run = _extract_or_raise(payload)
    _plot(run)

    log(f"wrote: {OUT_JSON}")
    log(f"wrote: {OUT_HOPS}")
    log(f"wrote: {OUT_RX}")


if __name__ == "__main__":
    main()

