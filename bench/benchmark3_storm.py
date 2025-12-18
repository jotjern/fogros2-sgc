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


def routing_states():
    try:
        r = rds()
        keys = r.keys("*-routing")
        out = []
        for k in keys:
            raw = r.get(k)
            if not raw:
                continue
            try:
                st = json.loads(raw)
            except Exception:
                continue
            out.append((k, st))
        r.close()
        return out
    except Exception:
        return []


def hop_histogram():
    hist = defaultdict(int)
    for _k, st in routing_states():
        pubs = (st or {}).get("publishers", []) or []
        if not pubs:
            continue
        root = pubs[0]
        proxies = set((st or {}).get("proxies", []) or [])
        pubs_set = set(pubs)
        edges = (st or {}).get("edges", []) or []
        children = defaultdict(list)
        for e in edges:
            p = (e or {}).get("parent")
            c = (e or {}).get("child")
            if p and c:
                children[p].append(c)
        dist = {root: 0}
        q = [root]
        while q:
            n = q.pop(0)
            for c in children.get(n, []):
                if c in dist:
                    continue
                dist[c] = dist[n] + 1
                q.append(c)
        for node, d in dist.items():
            if node in proxies or node in pubs_set:
                continue
            if d > 0:
                hist[d] += 1
    return dict(sorted(hist.items()))


def connected_listener_count():
    connected = set()
    for _k, st in routing_states():
        proxies = set((st or {}).get("proxies", []) or [])
        publishers = set((st or {}).get("publishers", []) or [])
        for e in (st or {}).get("edges", []) or []:
            child = (e or {}).get("child")
            if child and child not in proxies and child not in publishers:
                connected.add(child)
    return len(connected)


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


def main():
    import argparse

    ap = argparse.ArgumentParser()
    ap.add_argument("--listeners", type=int, default=50)
    ap.add_argument("--fanout", type=int, default=3)
    ap.add_argument("--mode", choices=["hierarchical", "direct"], default="hierarchical")
    ap.add_argument("--measure-interval-secs", type=float, default=1.0)
    ap.add_argument("--pre-secs", type=float, default=5.0)
    ap.add_argument("--down-secs", type=float, default=10.0)
    ap.add_argument("--post-secs", type=float, default=30.0)
    ap.add_argument("--storm-percent", type=float, default=50.0)
    ap.add_argument("--timeout-secs", type=int, default=240)
    ap.add_argument("--seed", type=int, default=1)
    args = ap.parse_args()

    tstamp = ts()
    out_json = os.path.join(OUT_DIR, f"benchmark3_storm_{args.mode}_fanout{args.fanout}_{tstamp}.json")
    out_hops = os.path.join(OUT_DIR, f"benchmark3_hops_{args.mode}_fanout{args.fanout}_{tstamp}.png")
    out_rx = os.path.join(OUT_DIR, f"benchmark3_listener_rx_{args.mode}_fanout{args.fanout}_{tstamp}.png")

    docker_cli = docker()

    proxy_count = 0 if args.mode == "direct" else choose_proxies(args.listeners, args.fanout)
    log("=== benchmark3: storm ===")
    log(f"  mode={args.mode} fanout={args.fanout} listeners={args.listeners} proxies={proxy_count}")
    log(f"  storm_percent={args.storm_percent} pre={args.pre_secs}s down={args.down_secs}s post={args.post_secs}s sample_interval={args.measure_interval_secs}s")
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

    rng = Random(int(args.seed))
    listener_containers = [(c.id, getattr(c, "container_name", "") or c.id) for c in docker_cli.compose.ps() if "listener" in (getattr(c, "container_name", "") or "")]
    rng.shuffle(listener_containers)
    k = int(round((max(0.0, min(100.0, float(args.storm_percent))) / 100.0) * len(listener_containers)))
    if len(listener_containers) > 0 and float(args.storm_percent) > 0:
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
        dt = max(0.001, float(args.measure_interval_secs))
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

    end_pre = time.time() + float(args.pre_secs)
    log("  phase=pre sampling...")
    while time.time() < end_pre:
        sample("pre")
        time.sleep(float(args.measure_interval_secs))

    log("  phase=down stopping listeners...")
    for cid, _ in down:
        try:
            docker_cli.container.stop(cid)
        except Exception:
            pass

    end_down = time.time() + float(args.down_secs)
    log("  phase=down sampling...")
    while time.time() < end_down:
        sample("down")
        time.sleep(float(args.measure_interval_secs))

    log("  phase=post starting listeners...")
    for cid, _ in down:
        try:
            docker_cli.container.start(cid)
        except Exception:
            pass

    end_post = time.time() + float(args.post_secs)
    log("  phase=post sampling...")
    while time.time() < end_post:
        sample("post")
        time.sleep(float(args.measure_interval_secs))

    log("  tearing down docker compose...")
    docker_cli.compose.down(volumes=True, remove_orphans=True)

    payload = {
        "meta": vars(args),
        "run": {
            "mode": args.mode,
            "fanout": args.fanout,
            "listeners": args.listeners,
            "proxies": proxy_count,
            "storm_percent": float(args.storm_percent),
            "down_count": len(down),
            "samples": samples,
        },
    }
    os.makedirs(OUT_DIR, exist_ok=True)
    with open(out_json, "w") as f:
        json.dump(payload, f, indent=2)

    plot_hops(payload["run"], out_hops)
    plot_listener_rx(payload["run"], out_rx)

    log(f"wrote: {out_json}")
    log(f"wrote: {out_hops}")
    log(f"wrote: {out_rx}")


if __name__ == "__main__":
    main()

