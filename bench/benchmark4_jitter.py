#!/usr/bin/env python3
import json
import math
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

LAT_RE = re.compile(r"BENCH_LATENCY_MS=([0-9]*\.?[0-9]+)")

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


def net_snapshot(docker_cli):
    out = {}
    ps = docker_cli.compose.ps()
    if not ps:
        return out
    stats = docker_cli.container.stats([c.id for c in ps])
    for s in stats:
        name = s.container_name
        parts = name.split("-")
        svc = parts[-2] if len(parts) >= 2 else name
        rx = getattr(s, "net_download", 0) or 0
        tx = getattr(s, "net_upload", 0) or 0
        try:
            rx = int(rx)
        except Exception:
            rx = 0
        try:
            tx = int(tx)
        except Exception:
            tx = 0
        out[name] = {"service": svc, "rx": rx, "tx": tx}
    return out


def net_delta(a, b):
    out = {}
    for name, v in (a or {}).items():
        w = (b or {}).get(name)
        if not w:
            continue
        rx = int(w.get("rx", 0)) - int(v.get("rx", 0))
        tx = int(w.get("tx", 0)) - int(v.get("tx", 0))
        if rx < 0:
            rx = 0
        if tx < 0:
            tx = 0
        out[name] = {"service": v.get("service"), "rx": rx, "tx": tx}
    return out


def parse_listener_latencies(docker_cli):
    out = {}
    for c in docker_cli.compose.ps():
        cname = getattr(c, "container_name", "") or c.id
        if "listener" not in cname:
            continue
        try:
            txt = docker_cli.container.logs(c.id)
        except Exception:
            txt = ""
        out[cname] = [float(m.group(1)) for m in LAT_RE.finditer(txt or "")]
    return out


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

    log(f"  measuring for {measure_secs}s...")
    a = net_snapshot(docker_cli)
    time.sleep(measure_secs)
    b = net_snapshot(docker_cli)
    d = net_delta(a, b)
    talker_tx = sum(int(v.get("tx", 0)) for v in d.values() if (v or {}).get("service") == "talker")
    secs = float(measure_secs)
    talker_tx_mbps = (talker_tx * 8.0) / (secs * 1e6) if secs > 0 else 0.0

    log("  parsing latency samples and computing jitter...")
    lat = parse_listener_latencies(docker_cli)
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

    docker_cli.compose.down(volumes=True, remove_orphans=True)
    time.sleep(2)

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


def main():
    import argparse

    ap = argparse.ArgumentParser()
    ap.add_argument("--listeners", default="1,5,10,15,25")
    ap.add_argument("--max-listeners", type=int, default=None)
    ap.add_argument("--step", type=int, default=None)
    ap.add_argument("--measure-secs", type=int, default=1)
    ap.add_argument("--timeout-secs", type=int, default=240)
    ap.add_argument("--fanout-hier", type=int, default=3)
    ap.add_argument("--fanout-direct", type=int, default=1000)
    ap.add_argument("--latency-hz", type=float, default=20.0)
    args = ap.parse_args()

    if args.max_listeners is not None:
        step = int(args.step or 1)
        xs = list(range(1, int(args.max_listeners) + 1, step))
    else:
        xs = [int(x.strip()) for x in str(args.listeners).split(",") if x.strip()]
    log("=== benchmark4: jitter sweep ===")
    log(f"  listeners={xs}")
    log(f"  measure_secs={args.measure_secs} timeout_secs={args.timeout_secs} latency_hz={args.latency_hz}")
    tstamp = ts()
    out_json = os.path.join(OUT_DIR, f"benchmark4_jitter_{tstamp}.json")
    out_std = os.path.join(OUT_DIR, f"benchmark4_jitter_std_{tstamp}.png")
    out_p95 = os.path.join(OUT_DIR, f"benchmark4_jitter_p95p50_{tstamp}.png")

    docker_cli = docker()
    results = {"meta": vars(args), "runs": []}

    for n in xs:
        results["runs"].append(
            run_case(
                docker_cli,
                mode="hierarchical",
                fanout=args.fanout_hier,
                listeners=n,
                measure_secs=args.measure_secs,
                timeout_secs=args.timeout_secs,
                latency_hz=args.latency_hz,
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
                measure_secs=args.measure_secs,
                timeout_secs=args.timeout_secs,
                latency_hz=args.latency_hz,
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

    d_std = [direct.get(x, {}).get("jitter", {}).get("std_avg_ms", 0.0) for x in xs]
    h_std = [hier.get(x, {}).get("jitter", {}).get("std_avg_ms", 0.0) for x in xs]
    d_p95 = [direct.get(x, {}).get("jitter", {}).get("p95_p50_ms", 0.0) for x in xs]
    h_p95 = [hier.get(x, {}).get("jitter", {}).get("p95_p50_ms", 0.0) for x in xs]

    plot_bar(xs, d_std, h_std, "Jitter vs listeners (stddev of latency)", "Stddev (ms)", out_std)
    plot_bar(xs, d_p95, h_p95, "Jitter vs listeners (p95 - p50)", "p95 - p50 (ms)", out_p95)

    log(f"wrote: {out_json}")
    log(f"wrote: {out_std}")
    log(f"wrote: {out_p95}")


if __name__ == "__main__":
    main()

