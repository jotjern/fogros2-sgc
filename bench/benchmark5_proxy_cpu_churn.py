#!/usr/bin/env python3
import json
import os
import time
from collections import defaultdict
from datetime import datetime
from random import Random

import redis
from python_on_whales import DockerClient


PROJECT_DIR = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
COMPOSE_FILE = os.path.join(PROJECT_DIR, "docker-compose.yaml")
OUT_DIR = os.path.join(PROJECT_DIR, "bench", "results")

REDIS_HOST = "localhost"
REDIS_PORT = 8002


def ts():
    return datetime.now().strftime("%Y%m%d_%H%M%S")

def log(msg):
    print(msg, flush=True)


def docker():
    return DockerClient(compose_files=[COMPOSE_FILE])


def rds():
    return redis.Redis(host=REDIS_HOST, port=REDIS_PORT, decode_responses=True)


def choose_proxies(num_listeners, fanout):
    f = int(fanout)
    if f <= 0:
        f = 1
    return max(1, ((num_listeners + f - 1) // f) * 2)


def wait_for_listeners(expected, timeout_secs):
    start = time.time()
    while time.time() - start < timeout_secs:
        try:
            r = rds()
            connected = set()
            for key in r.keys("*-routing"):
                raw = r.get(key)
                if not raw:
                    continue
                try:
                    st = json.loads(raw)
                except Exception:
                    continue
                proxies = set((st.get("proxies", []) or []))
                publishers = set((st.get("publishers", []) or []))
                for e in (st.get("edges", []) or []):
                    child = (e or {}).get("child")
                    if child and child not in proxies and child not in publishers:
                        connected.add(child)
            r.close()
            print(f"  {len(connected)}/{expected} listeners connected ({int(time.time()-start)}s)", flush=True)
            if len(connected) >= expected:
                return True
        except Exception:
            pass
        time.sleep(3)
    return False


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


def service_from_container_name(name):
    parts = (name or "").split("-")
    return parts[-2] if len(parts) >= 2 else name


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


def main():
    import argparse

    ap = argparse.ArgumentParser()
    ap.add_argument("--listeners", type=int, default=25)
    ap.add_argument("--fanout", type=int, default=3)
    ap.add_argument("--measure-secs", type=float, default=60.0)
    ap.add_argument("--sample-interval-secs", type=float, default=1.0)
    ap.add_argument("--reconnect-percent", type=float, default=10.0)
    ap.add_argument("--reconnect-interval-secs", type=float, default=5.0)
    ap.add_argument("--timeout-secs", type=int, default=240)
    ap.add_argument("--seed", type=int, default=1)
    args = ap.parse_args()

    tstamp = ts()
    out_json = os.path.join(OUT_DIR, f"benchmark5_proxy_cpu_churn_{tstamp}.json")
    out_png = os.path.join(OUT_DIR, f"benchmark5_proxy_cpu_churn_{tstamp}.png")

    docker_cli = docker()
    proxy_count = choose_proxies(args.listeners, args.fanout)

    log("=== benchmark5: proxy cpu under reconnect churn ===")
    log(f"  listeners={args.listeners} fanout={args.fanout} proxies={proxy_count}")
    log(f"  reconnect_percent={args.reconnect_percent}% every {args.reconnect_interval_secs}s")
    log(f"  duration={args.measure_secs}s sample_interval={args.sample_interval_secs}s")

    os.environ["FANOUT_FACTOR"] = str(args.fanout)
    os.environ["BENCH_LATENCY"] = "1"
    os.environ["BENCH_LATENCY_HZ"] = "10"

    log("  bringing up docker compose...")
    docker_cli.compose.down(volumes=True, remove_orphans=True)
    time.sleep(2)
    docker_cli.compose.up(detach=True, scales={"listener": args.listeners, "proxy": proxy_count})

    log("  waiting for all listeners to connect...")
    ok = wait_for_listeners(args.listeners, args.timeout_secs)
    if not ok:
        docker_cli.compose.down(volumes=True, remove_orphans=True)
        raise SystemExit("listeners did not connect in time")
    log("  all listeners connected")

    # identify proxies + listeners by inspecting compose ps + labels
    ps = docker_cli.compose.ps()
    proxy_ids = []
    listener_ids = []
    for c in ps:
        cid = c.id
        svc = getattr(c, "service", None) or getattr(c, "service_name", None)
        if not svc:
            try:
                info = docker_cli.container.inspect(cid)
                labels = getattr(getattr(info, "config", None), "labels", None) or {}
                svc = labels.get("com.docker.compose.service")
            except Exception:
                svc = None
        if svc == "proxy":
            proxy_ids.append(cid)
        if svc == "listener":
            listener_ids.append(cid)

    rng = Random(int(args.seed))
    samples = []
    t0 = time.time()
    next_reconnect = t0 + float(args.reconnect_interval_secs)

    end = t0 + float(args.measure_secs)
    last_status = 0
    while time.time() < end:
        now = time.time()
        if now >= next_reconnect:
            churn = pick_churn_set(listener_ids, args.reconnect_percent, rng)
            log(f"  churn: restarting {len(churn)} listeners...")
            for cid in churn:
                try:
                    docker_cli.container.restart(cid)
                except Exception:
                    pass
            next_reconnect = now + float(args.reconnect_interval_secs)

        cpu = proxy_cpu_snapshot(docker_cli, proxy_ids)
        samples.append({"t": now - t0, "proxy_cpu": cpu})
        if int(now - t0) != last_status and int(now - t0) % 5 == 0:
            last_status = int(now - t0)
            vals = list(cpu.values())
            avg = (sum(vals) / len(vals)) if vals else 0.0
            log(f"  t={int(now-t0)}s avg_proxy_cpu={avg:.2f}%")
        time.sleep(float(args.sample_interval_secs))

    log("  tearing down docker compose...")
    docker_cli.compose.down(volumes=True, remove_orphans=True)

    payload = {
        "meta": vars(args),
        "run": {
            "listeners": args.listeners,
            "fanout": args.fanout,
            "proxies": proxy_count,
            "samples": samples,
        },
    }
    os.makedirs(OUT_DIR, exist_ok=True)
    with open(out_json, "w") as f:
        json.dump(payload, f, indent=2)

    plot(samples, out_png)
    log(f"wrote: {out_json}")
    log(f"wrote: {out_png}")


if __name__ == "__main__":
    main()

