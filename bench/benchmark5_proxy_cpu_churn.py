#!/usr/bin/env python3
import json
import os
import time
from random import Random

from bench_utils import PROJECT_DIR, choose_proxies, docker, log, service_of, teardown, wait_for_listeners

OUT_DIR = os.path.join(PROJECT_DIR, "bench", "results")
OUT_JSON = os.path.join(OUT_DIR, "benchmark5_proxy_cpu_churn.json")
OUT_PNG = os.path.join(OUT_DIR, "benchmark5_proxy_cpu_churn.png")

# What we want (hardcoded):
LISTENERS = 25
FANOUT = 3
MEASURE_SECS = 60.0
SAMPLE_INTERVAL_SECS = 1.0
RECONNECT_PERCENT = 10.0
RECONNECT_INTERVAL_SECS = 5.0
TIMEOUT_SECS = 240
SEED = 1


def _cpu_percent_from_stats(s):
    cpu = getattr(s, "cpu_percentage", None)
    if cpu is None:
        cpu = getattr(s, "cpu_percent", 0.0)
    if isinstance(cpu, str) and cpu.endswith("%"):
        try:
            return float(cpu[:-1].strip())
        except Exception:
            return 0.0
    try:
        return float(cpu or 0.0)
    except Exception:
        return 0.0


def proxy_cpu_snapshot(docker_cli, proxy_ids):
    out = {}
    if not proxy_ids:
        return out
    stats = docker_cli.container.stats(proxy_ids)
    for s in stats:
        out[s.container_name] = _cpu_percent_from_stats(s)
    return out


def pick_churn_set(all_listener_ids, churn_percent, rng):
    if not all_listener_ids:
        return []
    p = max(0.0, min(100.0, float(churn_percent)))
    k = int(round((p / 100.0) * len(all_listener_ids)))
    if p > 0.0:
        k = max(1, k)
    k = min(k, len(all_listener_ids))
    ids = list(all_listener_ids)
    rng.shuffle(ids)
    return ids[:k]


def plot(samples, out_png):
    import matplotlib.pyplot as plt

    times = [s["t"] for s in samples]
    # union all proxies
    proxies = sorted({p for s in samples for p in (s.get("proxy_cpu") or {}).keys()})
    fig, ax = plt.subplots(figsize=(11.0, 5.2))

    # all proxy lines (faint)
    for p in proxies:
        ys = [(s.get("proxy_cpu") or {}).get(p, 0.0) for s in samples]
        ax.plot(times, ys, linewidth=1.0, alpha=0.25)

    # average line (bold)
    avg = []
    for s in samples:
        vals = list((s.get("proxy_cpu") or {}).values())
        avg.append((sum(vals) / len(vals)) if vals else 0.0)
    ax.plot(times, avg, linewidth=2.6, label="avg proxy cpu")

    ax.set_title("Proxy CPU usage over time (10% reconnect every 5s)")
    ax.set_xlabel("Time (s)")
    ax.set_ylabel("CPU (%)")
    ax.grid(True, axis="y", linestyle="--", linewidth=0.6, alpha=0.6)
    ax.legend()
    os.makedirs(os.path.dirname(out_png) or ".", exist_ok=True)
    fig.tight_layout()
    fig.savefig(out_png, dpi=160)


def _extract_or_raise(payload):
    if not isinstance(payload, dict):
        raise ValueError("payload must be a dict")
    run = payload.get("run")
    if not isinstance(run, dict):
        raise ValueError("payload must have a 'run' dict")
    samples = run.get("samples")
    if not isinstance(samples, list) or not samples:
        raise ValueError("run.samples missing/empty")
    if not any((s.get("proxy_cpu") or {}) for s in samples if isinstance(s, dict)):
        raise ValueError("samples have no proxy_cpu data")
    return samples


def main():
    docker_cli = docker()
    proxy_count = choose_proxies(LISTENERS, FANOUT)

    log("=== benchmark5: proxy cpu under reconnect churn ===")
    log(f"  listeners={LISTENERS} fanout={FANOUT} proxies={proxy_count}")
    log(f"  reconnect_percent={RECONNECT_PERCENT}% every {RECONNECT_INTERVAL_SECS}s")
    log(f"  duration={MEASURE_SECS}s sample_interval={SAMPLE_INTERVAL_SECS}s")

    if os.path.exists(OUT_JSON):
        try:
            with open(OUT_JSON, "r") as f:
                cached = json.load(f)
            samples = _extract_or_raise(cached)
            log(f"  using cached data: {OUT_JSON}")
            plot(samples, OUT_PNG)
            log(f"wrote: {OUT_PNG}")
            return
        except Exception as e:
            log(f"  cached data invalid, re-running benchmark: {e}")

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

    cache = {}
    ps = docker_cli.compose.ps()
    proxy_ids = [c.id for c in ps if service_of(docker_cli, c, cache) == "proxy"]
    listener_ids = [c.id for c in ps if service_of(docker_cli, c, cache) == "listener"]

    rng = Random(int(SEED))
    samples = []
    t0 = time.time()
    next_reconnect = t0 + float(RECONNECT_INTERVAL_SECS)

    end = t0 + float(MEASURE_SECS)
    last_status = 0
    while time.time() < end:
        now = time.time()
        if now >= next_reconnect:
            churn = pick_churn_set(listener_ids, RECONNECT_PERCENT, rng)
            log(f"  churn: restarting {len(churn)} listeners...")
            for cid in churn:
                try:
                    docker_cli.container.restart(cid)
                except Exception:
                    pass
            next_reconnect = now + float(RECONNECT_INTERVAL_SECS)

        cpu = proxy_cpu_snapshot(docker_cli, proxy_ids)
        samples.append({"t": now - t0, "proxy_cpu": cpu})
        if int(now - t0) != last_status and int(now - t0) % 5 == 0:
            last_status = int(now - t0)
            vals = list(cpu.values())
            avg = (sum(vals) / len(vals)) if vals else 0.0
            log(f"  t={int(now-t0)}s avg_proxy_cpu={avg:.2f}%")
        time.sleep(float(SAMPLE_INTERVAL_SECS))

    log("  tearing down docker compose...")
    teardown(docker_cli, volumes=True)

    payload = {
        "meta": {
            "listeners": LISTENERS,
            "fanout": FANOUT,
            "measure_secs": MEASURE_SECS,
            "sample_interval_secs": SAMPLE_INTERVAL_SECS,
            "reconnect_percent": RECONNECT_PERCENT,
            "reconnect_interval_secs": RECONNECT_INTERVAL_SECS,
            "timeout_secs": TIMEOUT_SECS,
            "seed": SEED,
        },
        "run": {
            "listeners": LISTENERS,
            "fanout": FANOUT,
            "proxies": proxy_count,
            "samples": samples,
        },
    }
    os.makedirs(OUT_DIR, exist_ok=True)
    with open(OUT_JSON, "w") as f:
        json.dump(payload, f, indent=2)

    samples = _extract_or_raise(payload)
    plot(samples, OUT_PNG)
    log(f"wrote: {OUT_JSON}")
    log(f"wrote: {OUT_PNG}")


if __name__ == "__main__":
    main()

