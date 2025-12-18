#!/usr/bin/env python3
import argparse
import json
import os
import re
import threading
import time
from collections import defaultdict
from datetime import datetime
from random import Random

import redis
from python_on_whales import DockerClient

DEFAULT_OUT = None
LAT_RE = re.compile(r"BENCH_LATENCY_MS=([0-9]*\\.?[0-9]+)")
SENT_RE = re.compile(r"BENCH_LATENCY_SENT_COUNT=([0-9]+)")

PROJECT_DIR = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
COMPOSE_FILE = os.path.join(PROJECT_DIR, "docker-compose.yaml")

REDIS_HOST = "localhost"
REDIS_PORT = 8002


def now_ts():
    return datetime.now().strftime("%Y%m%d_%H%M%S")


def _default_out_path():
    return f"bench/results/system_results_{now_ts()}.json"


def docker_client():
    return DockerClient(compose_files=[COMPOSE_FILE])


def get_redis():
    return redis.Redis(host=REDIS_HOST, port=REDIS_PORT, decode_responses=True)


def wait_for_listeners(expected, timeout_secs):
    start = time.time()
    while time.time() - start < timeout_secs:
        try:
            r = get_redis()
            connected = set()
            for key in r.keys("*-routing"):
                raw = r.get(key)
                if not raw:
                    continue
                try:
                    state = json.loads(raw)
                    proxies = set(state.get("proxies", []))
                    publishers = set(state.get("publishers", []))
                    for edge in state.get("edges", []):
                        child = edge.get("child")
                        if child and child not in proxies and child not in publishers:
                            connected.add(child)
                except Exception:
                    pass
            r.close()
            print(f"  {len(connected)}/{expected} listeners connected ({int(time.time()-start)}s)")
            if len(connected) >= expected:
                return True, connected
        except Exception:
            print(f"  waiting for redis... ({int(time.time()-start)}s)")
        time.sleep(3)
    return False, set()


def choose_num_proxies(proxy_mode, num_listeners, fanout):
    if proxy_mode == "none":
        return 0
    if proxy_mode == "one":
        return 1
    # auto: hierarchical routing needs enough proxy capacity to form a tree.
    # heuristic: ceil(n / fanout) * 2
    try:
        f = int(fanout)
    except Exception:
        f = 1
    if f <= 0:
        f = 1
    return max(1, ((num_listeners + f - 1) // f) * 2)


def compose_down(docker):
    docker.compose.down(volumes=True, remove_orphans=True)


def compose_up(docker, listener_count, proxy_count, extra_env=None):
    if extra_env:
        os.environ.update({k: str(v) for k, v in extra_env.items()})
    docker.compose.up(detach=True, scales={"listener": listener_count, "proxy": proxy_count})


def get_container_stats(docker):
    out = {}
    containers = docker.compose.ps()
    if not containers:
        return out
    stats_list = docker.container.stats([c.id for c in containers])
    for s in stats_list:
        name = s.container_name
        parts = name.split("-")
        svc = parts[-2] if len(parts) >= 2 else name
        cpu = getattr(s, "cpu_percentage", None)
        if cpu is None:
            cpu = getattr(s, "cpu_percent", 0.0)
        if isinstance(cpu, str) and cpu.endswith("%"):
            try:
                cpu = float(cpu[:-1].strip())
            except Exception:
                cpu = 0.0
        out[name] = {"service": svc, "cpu_percent": float(cpu or 0.0)}
    return out


def get_net_stats(docker):
    out = {}
    containers = docker.compose.ps()
    if not containers:
        return out
    stats_list = docker.container.stats([c.id for c in containers])
    for s in stats_list:
        name = s.container_name
        parts = name.split("-")
        svc = parts[-2] if len(parts) >= 2 else name
        rx = getattr(s, "net_download", None)
        tx = getattr(s, "net_upload", None)
        try:
            rx = int(rx or 0)
        except Exception:
            rx = 0
        try:
            tx = int(tx or 0)
        except Exception:
            tx = 0
        out[name] = {"service": svc, "rx": rx, "tx": tx}
    return out


def write_results_json(out_path, meta, runs):
    os.makedirs(os.path.dirname(out_path) or ".", exist_ok=True)
    with open(out_path, "w") as f:
        json.dump({"meta": meta, "runs": runs}, f, indent=2)


def _svc(container_name):
    parts = (container_name or "").split("-")
    return parts[-2] if len(parts) >= 2 else (container_name or "")


def _svc_of(docker, c, cache):
    s = getattr(c, "service", None)
    if s:
        return str(s)
    s = getattr(c, "service_name", None)
    if s:
        return str(s)

    cid = getattr(c, "id", None)
    if cid:
        if cid in cache:
            return cache[cid]
        try:
            info = docker.container.inspect(cid)
            labels = getattr(getattr(info, "config", None), "labels", None) or {}
            svc = labels.get("com.docker.compose.service")
            if svc:
                cache[cid] = svc
                return svc
        except Exception:
            pass

    cname = getattr(c, "container_name", "") or getattr(c, "name", "") or cid or ""
    return _svc(cname)


def _p(pct, vals):
    if not vals:
        return 0.0
    s = sorted(vals)
    k = int(round((pct / 100.0) * (len(s) - 1)))
    k = max(0, min(len(s) - 1, k))
    return float(s[k])


def _parse_lat(log_text):
    out = []
    for m in LAT_RE.finditer(log_text or ""):
        try:
            out.append(float(m.group(1)))
        except Exception:
            pass
    return out


def _routing_states():
    try:
        r = get_redis()
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


def _connected_listener_count():
    states = _routing_states()
    connected = set()
    for _k, st in states:
        proxies = set((st or {}).get("proxies", []) or [])
        publishers = set((st or {}).get("publishers", []) or [])
        for e in (st or {}).get("edges", []) or []:
            child = (e or {}).get("child")
            if child and child not in proxies and child not in publishers:
                connected.add(child)
    return len(connected)


def _hop_histogram():
    # aggregate across all topics
    hist = defaultdict(int)
    states = _routing_states()
    for _k, st in states:
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
            if not p or not c:
                continue
            children[p].append(c)

        # BFS distances from root
        dist = {root: 0}
        q = [root]
        while q:
            n = q.pop(0)
            for c in children.get(n, []):
                if c in dist:
                    continue
                dist[c] = dist[n] + 1
                q.append(c)

        # listeners are nodes that are not proxy/publisher and have a dist
        for node, d in dist.items():
            if node in proxies or node in pubs_set:
                continue
            if d > 0:
                hist[d] += 1
    return dict(sorted(hist.items()))


def _cpu_sample(docker, secs, interval):
    sums = defaultdict(float)
    counts = defaultdict(int)
    svc = {}
    start = time.time()
    while time.time() - start < secs:
        snap = get_container_stats(docker)
        for cname, meta in snap.items():
            sums[cname] += float(meta.get("cpu_percent", 0.0) or 0.0)
            counts[cname] += 1
            svc[cname] = meta.get("service")
        time.sleep(interval)
    per = {}
    by_svc = defaultdict(list)
    for cname, total in sums.items():
        c = counts.get(cname, 1)
        avg = total / c
        per[cname] = {"service": svc.get(cname), "avg_cpu_percent": avg}
        by_svc[svc.get(cname)].append(avg)
    svc_avg = {s: (sum(v) / len(v) if v else 0.0) for s, v in by_svc.items()}
    return per, svc_avg


def _lat_collect(docker):
    out = {}
    cache = {}
    for c in docker.compose.ps():
        if _svc_of(docker, c, cache) != "listener":
            continue
        cname = getattr(c, "container_name", "") or c.id
        try:
            txt = docker.container.logs(c.id)
        except Exception:
            txt = ""
        out[cname] = _parse_lat(txt)
    return out


def _sent_count(docker):
    last = None
    cache = {}
    for c in docker.compose.ps():
        if _svc_of(docker, c, cache) != "talker":
            continue
        try:
            txt = docker.container.logs(c.id)
        except Exception:
            txt = ""
        for m in SENT_RE.finditer(txt or ""):
            try:
                last = int(m.group(1))
            except Exception:
                pass
        break
    return last


def _chaos_targets(docker, services, pct, rng):
    cand = []
    cache = {}
    for c in docker.compose.ps():
        svc = _svc_of(docker, c, cache)
        if svc in services:
            cname = getattr(c, "container_name", "") or c.id
            cand.append((c.id, cname))
    if not cand or pct <= 0:
        return []
    pct = max(0.0, min(100.0, pct))
    k = int(round((pct / 100.0) * len(cand)))
    if pct > 0:
        k = max(1, k)
    k = min(k, len(cand))
    rng.shuffle(cand)
    return cand[:k]


def _chaos_loop(docker, targets, interval, jitter, rng, stop_evt, events):
    if not targets:
        return
    interval = max(0.5, float(interval))
    jitter = max(0.0, float(jitter))
    while not stop_evt.is_set():
        j = 1.0 if jitter == 0 else rng.uniform(max(0.1, 1.0 - jitter), 1.0 + jitter)
        stop_evt.wait(interval * j)
        if stop_evt.is_set():
            break
        cid, cname = targets[rng.randrange(0, len(targets))]
        t0 = time.time()
        try:
            docker.container.restart(cid)
            events.append({"ts": t0, "container_name": cname, "ok": True})
        except Exception as e:
            events.append({"ts": t0, "container_name": cname, "ok": False, "error": str(e)})


def _join_latency(docker, start_ts, max_wait):
    listeners = []
    cache = {}
    for c in docker.compose.ps():
        if _svc_of(docker, c, cache) == "listener":
            cname = getattr(c, "container_name", "") or c.id
            listeners.append((c.id, cname))

    first = {}
    pending = {cid: cname for cid, cname in listeners}
    end = start_ts + max_wait
    while pending and time.time() < end:
        if int(time.time() - start_ts) % 3 == 0:
            # lightweight progress so it doesn't look "stuck"
            done_n = len(first)
            total_n = len(listeners)
            print(f"  join: {done_n}/{total_n} listeners received first message ({int(time.time()-start_ts)}s)", flush=True)
        done = []
        for cid, cname in pending.items():
            try:
                txt = docker.container.logs(cid)
            except Exception:
                txt = ""
            if LAT_RE.search(txt or ""):
                first[cname] = time.time() - start_ts
                done.append(cid)
        for cid in done:
            pending.pop(cid, None)
        if pending:
            time.sleep(0.5)

    return first, list(pending.values())


def _proxy_counts(docker):
    running = 0
    cache = {}
    for c in docker.compose.ps():
        if _svc_of(docker, c, cache) == "proxy":
            running += 1

    registered = 0
    try:
        r = get_redis()
        proxies = set()
        for key in r.keys("*-routing"):
            raw = r.get(key)
            if not raw:
                continue
            try:
                st = json.loads(raw)
                for p in st.get("proxies", []) or []:
                    proxies.add(p)
            except Exception:
                pass
        r.close()
        registered = len(proxies)
    except Exception:
        registered = 0

    return running, registered


def _count_lat_samples(txt):
    return len(list(LAT_RE.finditer(txt or "")))


def _rejoin_latency_after_restart(docker, ids, max_wait=120):
    # measure: time from start() call to first NEW latency sample line
    before = {}
    for cid, _name in ids:
        try:
            before[cid] = _count_lat_samples(docker.container.logs(cid))
        except Exception:
            before[cid] = 0

    start_ts = {}
    for cid, _name in ids:
        start_ts[cid] = time.time()
        try:
            docker.container.start(cid)
        except Exception:
            pass

    pending = set(cid for cid, _ in ids)
    out = {}
    end = time.time() + max_wait
    while pending and time.time() < end:
        done = []
        for cid in list(pending):
            try:
                txt = docker.container.logs(cid)
            except Exception:
                txt = ""
            if _count_lat_samples(txt) > before.get(cid, 0):
                out[cid] = time.time() - start_ts.get(cid, time.time())
                done.append(cid)
        for cid in done:
            pending.discard(cid)
        if pending:
            time.sleep(0.5)
    return out, list(pending)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--fanouts", default="3,1000")
    ap.add_argument("--listeners", default="50")
    ap.add_argument("--proxy-mode", choices=["auto", "one", "none"], default="auto")
    ap.add_argument("--proxies", type=int, default=None)
    ap.add_argument("--measure-secs", type=int, default=30)
    ap.add_argument("--timeout-secs", type=int, default=240)
    ap.add_argument("--sample-interval-secs", type=float, default=1.0)
    ap.add_argument("--latency-hz", type=float, default=20.0)
    ap.add_argument("--join-max-wait-secs", type=float, default=60.0)
    ap.add_argument("--chaos-restart", action="store_true")
    ap.add_argument("--chaos-percent", type=float, default=100.0)
    ap.add_argument("--chaos-interval-secs", type=float, default=10.0)
    ap.add_argument("--chaos-jitter", type=float, default=0.3)
    ap.add_argument("--chaos-services", default="listener,proxy")
    ap.add_argument("--chaos-seed", type=int, default=1)
    ap.add_argument("--storm", action="store_true")
    ap.add_argument("--storm-percent", type=float, default=50.0)
    ap.add_argument("--storm-down-secs", type=float, default=10.0)
    ap.add_argument("--storm-pre-secs", type=float, default=5.0)
    ap.add_argument("--storm-post-secs", type=float, default=30.0)
    ap.add_argument("--storm-sample-interval-secs", type=float, default=1.0)
    ap.add_argument("--storm-seed", type=int, default=1)
    ap.add_argument("--out", default=DEFAULT_OUT)
    args = ap.parse_args()

    fanouts = [int(x.strip()) for x in args.fanouts.split(",") if x.strip()]
    listener_counts = [int(x.strip()) for x in args.listeners.split(",") if x.strip()]

    docker = docker_client()
    meta = {
        "schema": "fogros2-sgc.bench.system_results.v1",
        "created_at": now_ts(),
        "measure_secs": args.measure_secs,
        "timeout_secs": args.timeout_secs,
        "fanouts": fanouts,
        "listeners": listener_counts,
        "proxy_mode": args.proxy_mode,
        "proxies": args.proxies,
        "latency_hz": args.latency_hz,
        "chaos_restart": bool(args.chaos_restart),
        "chaos_percent": float(args.chaos_percent),
        "chaos_interval_secs": float(args.chaos_interval_secs),
        "chaos_jitter": float(args.chaos_jitter),
        "chaos_services": args.chaos_services,
        "chaos_seed": int(args.chaos_seed),
        "storm": bool(args.storm),
        "storm_percent": float(args.storm_percent),
        "storm_down_secs": float(args.storm_down_secs),
        "storm_pre_secs": float(args.storm_pre_secs),
        "storm_post_secs": float(args.storm_post_secs),
        "storm_sample_interval_secs": float(args.storm_sample_interval_secs),
        "storm_seed": int(args.storm_seed),
    }

    out_path = args.out or _default_out_path()
    runs = []
    write_results_json(out_path, meta, runs)

    for fanout in fanouts:
        for n in listener_counts:
            proxy_count = args.proxies if args.proxies is not None else choose_num_proxies(args.proxy_mode, n, fanout)
            print(f"\n{'='*50}\nSystem Benchmark: fanout={fanout} listeners={n} proxies={proxy_count}\n{'='*50}")

            try:
                compose_down(docker)
                time.sleep(2)
                t_up = time.time()
                compose_up(
                    docker,
                    listener_count=n,
                    proxy_count=proxy_count,
                    extra_env={
                        "FANOUT_FACTOR": str(fanout),
                        "BENCH_LATENCY": "1",
                        "BENCH_LATENCY_HZ": str(args.latency_hz),
                    },
                )

                ok, connected = wait_for_listeners(n, args.timeout_secs)
                if not ok:
                    runs.append({"fanout": fanout, "num_listeners": n, "num_proxies": proxy_count, "success": False})
                    write_results_json(out_path, meta, runs)
                    continue
                connect_secs = time.time() - t_up
                proxy_running, proxy_registered = _proxy_counts(docker)

                print("  measuring join latency (first message per listener)...", flush=True)
                join_first, join_missing = _join_latency(
                    docker,
                    t_up,
                    max_wait=min(float(args.timeout_secs), float(args.join_max_wait_secs)),
                )
                join_vals = list(join_first.values())
                join_summary = {
                    "count": len(join_vals),
                    "missing": len(join_missing),
                    "p50_secs": _p(50, join_vals),
                    "p95_secs": _p(95, join_vals),
                    "max_secs": max(join_vals) if join_vals else 0.0,
                }

                storm = None
                if args.storm:
                    # pick 50% of listeners, stop them, wait, then start and sample time series
                    rng_storm = Random(int(args.storm_seed))
                    listeners = []
                    for c in docker.compose.ps():
                        if _svc_of(c) == "listener":
                            listeners.append((c.id, getattr(c, "container_name", "") or c.id))
                    rng_storm.shuffle(listeners)
                    k = int(round((max(0.0, min(100.0, float(args.storm_percent))) / 100.0) * len(listeners)))
                    if len(listeners) > 0 and float(args.storm_percent) > 0:
                        k = max(1, k)
                    k = min(k, len(listeners))
                    down = listeners[:k]

                    # baseline sample for pre secs
                    samples = []
                    sample_start = time.time()
                    last_net = get_net_stats(docker)

                    def take_sample(phase):
                        nonlocal last_net
                        now = time.time()
                        cur = get_net_stats(docker)
                        delta = {}
                        for cname, a in (last_net or {}).items():
                            b = (cur or {}).get(cname)
                            if not b:
                                continue
                            rx = int(b.get("rx", 0)) - int(a.get("rx", 0))
                            tx = int(b.get("tx", 0)) - int(a.get("tx", 0))
                            if rx < 0:
                                rx = 0
                            if tx < 0:
                                tx = 0
                            delta[cname] = {"service": a.get("service"), "rx": rx, "tx": tx}
                        last_net = cur

                        talker_tx = sum(int(v.get("tx", 0)) for v in delta.values() if (v or {}).get("service") == "talker")
                        listener_rx = sum(int(v.get("rx", 0)) for v in delta.values() if (v or {}).get("service") == "listener")
                        dt = max(0.001, float(args.storm_sample_interval_secs))
                        talker_tx_mbps = (talker_tx * 8.0) / (dt * 1e6)
                        listener_rx_mbps = (listener_rx * 8.0) / (dt * 1e6)

                        samples.append(
                            {
                                "t": now - sample_start,
                                "phase": phase,
                                "talker_tx_mbps": talker_tx_mbps,
                                "listener_rx_mbps": listener_rx_mbps,
                                "connected_listeners": _connected_listener_count(),
                                "hops": _hop_histogram(),
                            }
                        )

                    # pre
                    end_pre = sample_start + float(args.storm_pre_secs)
                    while time.time() < end_pre:
                        take_sample("pre")
                        time.sleep(float(args.storm_sample_interval_secs))

                    # stop
                    for cid, _name in down:
                        try:
                            docker.container.stop(cid)
                        except Exception:
                            pass
                    stop_ts = time.time()

                    end_down = stop_ts + float(args.storm_down_secs)
                    while time.time() < end_down:
                        take_sample("down")
                        time.sleep(float(args.storm_sample_interval_secs))

                    # start and measure rejoin latency
                    rejoin_lat, rejoin_missing = _rejoin_latency_after_restart(docker, down, max_wait=120)

                    end_post = time.time() + float(args.storm_post_secs)
                    while time.time() < end_post:
                        take_sample("post")
                        time.sleep(float(args.storm_sample_interval_secs))

                    vals = list(rejoin_lat.values())
                    storm = {
                        "percent": float(args.storm_percent),
                        "down_secs": float(args.storm_down_secs),
                        "pre_secs": float(args.storm_pre_secs),
                        "post_secs": float(args.storm_post_secs),
                        "sample_interval_secs": float(args.storm_sample_interval_secs),
                        "down_count": len(down),
                        "rejoin_latency_secs": {name: rejoin_lat.get(cid) for cid, name in down},
                        "rejoin_missing": [name for cid, name in down if cid in rejoin_missing],
                        "rejoin_summary": {
                            "count": len(vals),
                            "mean_secs": (sum(vals) / len(vals)) if vals else 0.0,
                            "p50_secs": _p(50, vals),
                            "p95_secs": _p(95, vals),
                            "max_secs": max(vals) if vals else 0.0,
                        },
                        "samples": samples,
                    }

                rng = Random(int(args.chaos_seed))
                chaos_services = {s.strip() for s in args.chaos_services.split(",") if s.strip()}
                targets = _chaos_targets(docker, chaos_services, float(args.chaos_percent), rng)
                events = []
                stop_evt = threading.Event()
                t = None
                if args.chaos_restart:
                    if not targets:
                        raise RuntimeError("chaos enabled but no targets")
                    t = threading.Thread(
                        target=_chaos_loop,
                        args=(docker, targets, args.chaos_interval_secs, args.chaos_jitter, rng, stop_evt, events),
                        daemon=True,
                    )
                    t.start()

                print(f"  sampling cpu/net for {args.measure_secs}s...", flush=True)
                net1 = get_net_stats(docker)
                cpu_by_container, cpu_by_service = _cpu_sample(docker, args.measure_secs, args.sample_interval_secs)
                net2 = get_net_stats(docker)
                stop_evt.set()
                if t is not None:
                    t.join(timeout=5)

                net_delta = {}
                for cname, a in (net1 or {}).items():
                    b = (net2 or {}).get(cname)
                    if not b:
                        continue
                    rx = int(b.get("rx", 0)) - int(a.get("rx", 0))
                    tx = int(b.get("tx", 0)) - int(a.get("tx", 0))
                    # container restarts can reset counters; don't go negative
                    if rx < 0:
                        rx = 0
                    if tx < 0:
                        tx = 0
                    net_delta[cname] = {"service": a.get("service"), "rx": rx, "tx": tx}

                talker_tx = 0
                total_tx = 0
                for meta2 in net_delta.values():
                    tx = int((meta2 or {}).get("tx", 0))
                    total_tx += tx
                    if (meta2 or {}).get("service") == "talker":
                        talker_tx += tx
                measure_secs = float(args.measure_secs)
                talker_tx_mbps = (talker_tx * 8.0) / (measure_secs * 1e6) if measure_secs > 0 else 0.0
                talker_tx_mbps_per_listener = talker_tx_mbps / n if n > 0 else 0.0

                sent = _sent_count(docker)
                lat_by_container = _lat_collect(docker)
                all_lat = [v for vs in lat_by_container.values() for v in vs]
                if not all_lat:
                    runs.append(
                        {
                            "fanout": fanout,
                            "num_listeners": n,
                            "num_proxies": proxy_count,
                            "proxy_running": proxy_running,
                            "proxy_registered": proxy_registered,
                            "success": False,
                            "error": "no latency samples",
                            "join": {
                                "connected_secs": connect_secs,
                                "first_packet_secs_per_listener": join_first,
                                "missing_listeners": join_missing,
                                "summary": join_summary,
                            },
                            "cpu": {"containers": cpu_by_container, "service_avg_cpu_percent": cpu_by_service},
                            "bandwidth": {
                                "containers": net_delta,
                                "talker_tx_mbps": talker_tx_mbps,
                                "talker_tx_mbps_per_listener": talker_tx_mbps_per_listener,
                                "total_tx_bytes": total_tx,
                            },
                            "chaos": {"targets": [name for _, name in targets], "events": events},
                            "delivery": {"sent": sent, "received_per_listener": {k: len(v) for k, v in lat_by_container.items()}},
                        }
                    )
                    write_results_json(out_path, meta, runs)
                    continue

                lat_summary = {
                    "count": len(all_lat),
                    "mean_ms": sum(all_lat) / len(all_lat),
                    "p50_ms": _p(50, all_lat),
                    "p95_ms": _p(95, all_lat),
                    "p99_ms": _p(99, all_lat),
                    "max_ms": max(all_lat),
                }
                per_listener_mean = {k: (sum(v) / len(v) if v else 0.0) for k, v in lat_by_container.items()}
                means = list(per_listener_mean.values())
                avg_listener_mean = sum(means) / len(means) if means else 0.0

                recv_counts = {k: len(v) for k, v in lat_by_container.items()}
                if sent is None:
                    sent = int(args.measure_secs * args.latency_hz)
                drop = {k: max(0, sent - c) for k, c in recv_counts.items()}
                ratio = {k: (c / sent if sent > 0 else 0.0) for k, c in recv_counts.items()}
                runs.append(
                    {
                        "fanout": fanout,
                        "num_listeners": n,
                        "num_proxies": proxy_count,
                        "proxy_running": proxy_running,
                        "proxy_registered": proxy_registered,
                        "success": True,
                        "join": {
                            "connected_secs": connect_secs,
                            "first_packet_secs_per_listener": join_first,
                            "missing_listeners": join_missing,
                            "summary": join_summary,
                        },
                        "storm": storm,
                        "cpu": {"containers": cpu_by_container, "service_avg_cpu_percent": cpu_by_service},
                        "bandwidth": {
                            "containers": net_delta,
                            "talker_tx_mbps": talker_tx_mbps,
                            "talker_tx_mbps_per_listener": talker_tx_mbps_per_listener,
                            "total_tx_bytes": total_tx,
                        },
                        "chaos": {"targets": [name for _, name in targets], "events": events},
                        "delivery": {"sent": sent, "received_per_listener": recv_counts, "dropped_est_per_listener": drop, "receive_ratio_per_listener": ratio},
                        "latency": {"summary": lat_summary, "avg_per_listener_mean_ms": avg_listener_mean, "per_listener_mean_ms": per_listener_mean},
                    }
                )
                write_results_json(out_path, meta, runs)
            finally:
                try:
                    compose_down(docker)
                except Exception:
                    pass
                time.sleep(2)

    print(out_path)


if __name__ == "__main__":
    main()

