#!/usr/bin/env python3
import json
import os
import re
import time
from datetime import datetime

import redis
from python_on_whales import DockerClient


PROJECT_DIR = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
COMPOSE_FILE = os.path.join(PROJECT_DIR, "docker-compose.yaml")
OUT_DIR = os.path.join(PROJECT_DIR, "bench", "results")

REDIS_HOST = "localhost"
REDIS_PORT = 8002

def log(msg):
    print(msg, flush=True)


def ts():
    return datetime.now().strftime("%Y%m%d_%H%M%S")


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


def container_hostname(docker_cli, cid):
    try:
        info = docker_cli.container.inspect(cid)
        hn = getattr(getattr(info, "config", None), "hostname", None)
        if hn:
            return str(hn)
    except Exception:
        pass
    return str(cid)[:12]


def join_latency_from_redis(hostnames, max_wait_secs):
    # join latency = bench_first_msg_ms - bench_join_attempt_ms
    deadline = time.time() + float(max_wait_secs)
    out_ms = {}
    last_progress = 0
    while time.time() < deadline and len(out_ms) < len(hostnames):
        now = time.time()
        try:
            r = rds()
            for hn in hostnames:
                if hn in out_ms:
                    continue
                a = r.hget("bench_join_attempt_ms", hn)
                b = r.hget("bench_first_msg_ms", hn)
                if a is None or b is None:
                    continue
                try:
                    out_ms[hn] = int(b) - int(a)
                except Exception:
                    pass
            r.close()
        except Exception:
            pass

        if int(deadline - now) != last_progress and int(now) % 3 == 0:
            last_progress = int(deadline - now)
            log(f"  join: got first message from {len(out_ms)}/{len(hostnames)} listeners so far...")
        time.sleep(0.5)
    return out_ms


def run_case(docker_cli, *, mode, fanout, listeners, timeout_secs, latency_hz, join_wait_secs):
    if mode == "direct":
        proxy_count = 0
    else:
        proxy_count = choose_proxies(listeners, fanout)

    log(f"\n=== benchmark2: {mode} fanout={fanout} listeners={listeners} proxies={proxy_count} ===")

    os.environ["FANOUT_FACTOR"] = str(fanout)
    os.environ["BENCH_LATENCY"] = "1"
    os.environ["BENCH_LATENCY_HZ"] = str(latency_hz)

    docker_cli.compose.down(volumes=True, remove_orphans=True)
    time.sleep(2)
    log("  bringing up docker compose...")
    docker_cli.compose.up(detach=True, scales={"listener": listeners, "proxy": proxy_count})

    log("  waiting for all listeners to connect...")
    ok = wait_for_listeners(listeners, timeout_secs)
    if not ok:
        log("  ERROR: listeners did not connect in time")
        docker_cli.compose.down(volumes=True, remove_orphans=True)
        return {"success": False, "error": "listeners did not connect in time"}
    log("  all listeners connected")

    # Determine per-listener hostname (used by routing/db benchmark hooks)
    hostnames = []
    for c in docker_cli.compose.ps():
        s = getattr(c, "service", None) or getattr(c, "service_name", None)
        if s != "listener":
            continue
        hostnames.append(container_hostname(docker_cli, c.id))

    log(f"  measuring join latency from routing subscribe -> first received message (waiting up to {join_wait_secs}s)...")
    join_ms = join_latency_from_redis(hostnames, join_wait_secs)
    vals = list(join_ms.values())
    avg = (sum(vals) / len(vals)) if vals else None
    log(f"  join done: got={len(vals)} missing={max(0, len(hostnames) - len(vals))}")

    docker_cli.compose.down(volumes=True, remove_orphans=True)
    time.sleep(2)

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


def main():
    import argparse

    ap = argparse.ArgumentParser()
    ap.add_argument("--listeners", default="1,5,10,15,25")
    ap.add_argument("--max-listeners", type=int, default=None)
    ap.add_argument("--step", type=int, default=None)
    ap.add_argument("--timeout-secs", type=int, default=240)
    ap.add_argument("--fanout-hier", type=int, default=3)
    ap.add_argument("--fanout-direct", type=int, default=1000)
    ap.add_argument("--latency-hz", type=float, default=10.0)
    ap.add_argument("--join-wait-secs", type=float, default=60.0)
    args = ap.parse_args()

    if args.max_listeners is not None:
        step = int(args.step or 1)
        xs = list(range(1, int(args.max_listeners) + 1, step))
    else:
        xs = [int(x.strip()) for x in str(args.listeners).split(",") if x.strip()]

    log("=== benchmark2: join latency sweep ===")
    log(f"  listeners={xs}")
    log(f"  latency_hz={args.latency_hz} join_wait_secs={args.join_wait_secs} timeout_secs={args.timeout_secs}")
    log(f"  hierarchical fanout={args.fanout_hier} | direct fanout={args.fanout_direct} (proxies=0)")
    tstamp = ts()
    out_json = os.path.join(OUT_DIR, f"benchmark2_join_latency_{tstamp}.json")
    out_plot = os.path.join(OUT_DIR, f"benchmark2_join_latency_{tstamp}.png")

    docker_cli = docker()
    results = {"meta": vars(args), "runs": []}

    for n in xs:
        results["runs"].append(
            run_case(
                docker_cli,
                mode="hierarchical",
                fanout=args.fanout_hier,
                listeners=n,
                timeout_secs=args.timeout_secs,
                latency_hz=args.latency_hz,
                join_wait_secs=args.join_wait_secs,
            )
        )
        with open(out_json, "w") as f:
            json.dump(results, f, indent=2)

        results["runs"].append(
            run_case(
                docker_cli,
                mode="direct",
                fanout=args.fanout_direct,
                listeners=n,
                timeout_secs=args.timeout_secs,
                latency_hz=args.latency_hz,
                join_wait_secs=args.join_wait_secs,
            )
        )
        with open(out_json, "w") as f:
            json.dump(results, f, indent=2)

    hier = {}
    direct = {}
    for r in results["runs"]:
        if not r.get("success"):
            continue
        if r["mode"] == "hierarchical":
            hier[r["listeners"]] = r
        if r["mode"] == "direct":
            direct[r["listeners"]] = r

    d_vals = [float((direct.get(x, {}).get("join", {}) or {}).get("avg_secs", 0.0) or 0.0) * 1000.0 for x in xs]
    h_vals = [float((hier.get(x, {}).get("join", {}) or {}).get("avg_secs", 0.0) or 0.0) * 1000.0 for x in xs]

    plot_bar(xs, d_vals, h_vals, out_plot, ideal_ms=50.0)

    log(f"wrote: {out_json}")
    log(f"wrote: {out_plot}")


if __name__ == "__main__":
    main()

