#!/usr/bin/env python3
"""Shared utilities for FogROS2-SGC benchmarks."""
import json
import math
import os
import re
import time
from collections import defaultdict

import redis
from python_on_whales import DockerClient


PROJECT_DIR = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
COMPOSE_FILE = os.path.join(PROJECT_DIR, "docker-compose.yaml")

REDIS_HOST = "localhost"
REDIS_PORT = 8002

LAT_RE = re.compile(r"BENCH_LATENCY_MS=([0-9]*\.?[0-9]+)")

# Publication-quality plot styling
PLOT_STYLE = {
    "colors": {
        "direct": "#D62728",      # Red (colorblind-safe)
        "hierarchical": "#1F77B4", # Blue (colorblind-safe)
        "neutral": "#7F7F7F",      # Gray
    },
    "figsize": (8, 5),
    "dpi": 300,
    "font_size": 11,
    "title_size": 13,
    "legend_size": 10,
    "line_width": 2,
    "marker_size": 7,
    "bar_width": 0.35,
}


def setup_plot_style():
    """Configure matplotlib for publication-quality figures."""
    import matplotlib.pyplot as plt
    plt.rcParams.update({
        "font.size": PLOT_STYLE["font_size"],
        "axes.titlesize": PLOT_STYLE["title_size"],
        "axes.labelsize": PLOT_STYLE["font_size"],
        "xtick.labelsize": PLOT_STYLE["font_size"] - 1,
        "ytick.labelsize": PLOT_STYLE["font_size"] - 1,
        "legend.fontsize": PLOT_STYLE["legend_size"],
        "figure.figsize": PLOT_STYLE["figsize"],
        "figure.dpi": PLOT_STYLE["dpi"],
        "savefig.dpi": PLOT_STYLE["dpi"],
        "axes.grid": True,
        "grid.alpha": 0.3,
        "grid.linestyle": "--",
    })


def log(msg):
    print(msg, flush=True)


def docker():
    return DockerClient(compose_files=[COMPOSE_FILE])


def rds():
    return redis.Redis(host=REDIS_HOST, port=REDIS_PORT, decode_responses=True)


def service_of(docker_cli, c, cache):
    s = getattr(c, "service", None) or getattr(c, "service_name", None)
    if s:
        return str(s)

    cid = getattr(c, "id", None)
    if cid:
        if cid in cache:
            return cache[cid]
        try:
            info = docker_cli.container.inspect(cid)
            labels = getattr(getattr(info, "config", None), "labels", None) or {}
            svc = labels.get("com.docker.compose.service")
            if svc:
                cache[cid] = svc
                return svc
        except Exception:
            pass

    name = getattr(c, "container_name", "") or getattr(c, "name", "") or cid or ""
    parts = name.split("-")
    return parts[-2] if len(parts) >= 2 else name


def compose_up(docker_cli, *, listeners, proxies, env):
    os.environ.update({k: str(v) for k, v in (env or {}).items()})
    docker_cli.compose.up(detach=True, scales={"listener": int(listeners), "proxy": int(proxies)})


def teardown(docker_cli, *, volumes=True):
    docker_cli.compose.down(volumes=bool(volumes), remove_orphans=True)
    time.sleep(2)


def wait_for_listeners(expected, timeout_secs):
    start = time.time()
    while time.time() - start < timeout_secs:
        connected = set()
        try:
            r = rds()
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
        except Exception as e:
            log(f"  waiting for redis... ({int(time.time()-start)}s) ({e})")
            time.sleep(3)
            continue

        log(f"  {len(connected)}/{expected} listeners connected ({int(time.time()-start)}s)")
        if len(connected) >= expected:
            return True
        time.sleep(3)
    return False


def warm_up(secs=5):
    """Wait for system to stabilize after connections are established."""
    log(f"  warming up for {secs}s...")
    time.sleep(secs)


def choose_proxies(num_listeners, fanout):
    """Calculate appropriate proxy count for hierarchical routing.
    
    For a tree with fanout F and N listeners, we need intermediate nodes:
    - N/F nodes at the leaf-parent level
    - N/F² nodes at the next level up
    - etc.
    Total ≈ N/(F-1) for the geometric series, plus headroom.
    """
    f = max(2, int(fanout))
    n = max(1, int(num_listeners))
    
    if n <= f:
        # All listeners can attach directly to publisher
        return 0
    
    # Geometric series: need N/(F-1) proxies, with 50% headroom for CAS conflicts
    proxies = int(math.ceil(n / (f - 1) * 1.5))
    
    # Minimum: enough for at least 2 levels of proxies
    min_proxies = f + f * f
    proxies = max(proxies, min_proxies)
    
    return proxies


def container_hostname(docker_cli, cid):
    try:
        info = docker_cli.container.inspect(cid)
        hn = getattr(getattr(info, "config", None), "hostname", None)
        if hn:
            return str(hn)
    except Exception:
        pass
    return str(cid)[:12]


def listener_hostnames(docker_cli):
    cache = {}
    out = []
    for c in docker_cli.compose.ps():
        if service_of(docker_cli, c, cache) != "listener":
            continue
        out.append(container_hostname(docker_cli, c.id))
    return out


def join_latency_from_redis(hostnames, max_wait_secs):
    """Get join latency (time from subscribe attempt to first message) from Redis."""
    deadline = time.time() + float(max_wait_secs)
    out_ms = {}
    while time.time() < deadline and len(out_ms) < len(hostnames):
        try:
            r = rds()
            for hn in hostnames:
                if hn in out_ms:
                    continue
                a = r.hget("bench_join_attempt_ms", hn)
                b = r.hget("bench_first_msg_ms", hn)
                if a is None or b is None:
                    continue
                out_ms[hn] = int(b) - int(a)
            r.close()
        except Exception:
            pass
        time.sleep(0.5)
    return out_ms


def net_snapshot(docker_cli):
    """Get network stats for all containers."""
    out = {}
    try:
        ps = docker_cli.compose.ps()
        if not ps:
            return out
        
        cache = {}
        name_to_service = {
            (getattr(c, "container_name", None) or getattr(c, "name", None) or c.id): service_of(docker_cli, c, cache)
            for c in ps
        }
        
        stats = docker_cli.container.stats([c.id for c in ps])
        
        for s in stats:
            name = s.container_name
            svc = name_to_service.get(name) or "unknown"
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
        
    except Exception as e:
        log(f"  net_snapshot warning: {e}")
        return out


def net_delta(a, b):
    """Compute network stats delta between two snapshots."""
    out = {}
    for name, v in (a or {}).items():
        w = (b or {}).get(name)
        if not w:
            continue
        rx = max(0, int(w.get("rx", 0)) - int(v.get("rx", 0)))
        tx = max(0, int(w.get("tx", 0)) - int(v.get("tx", 0)))
        out[name] = {"service": v.get("service"), "rx": rx, "tx": tx}
    return out


def measure_net(docker_cli, measure_secs):
    """Measure network throughput over a time period."""
    a = net_snapshot(docker_cli)
    time.sleep(float(measure_secs))
    b = net_snapshot(docker_cli)
    d = net_delta(a, b)
    secs = float(measure_secs)
    
    talker_tx = sum(int(v.get("tx", 0)) for v in d.values() if (v or {}).get("service") == "talker")
    listener_rx = sum(int(v.get("rx", 0)) for v in d.values() if (v or {}).get("service") == "listener")
    
    talker_tx_mbps = (talker_tx * 8.0) / (secs * 1e6) if secs > 0 else 0.0
    listener_rx_mbps = (listener_rx * 8.0) / (secs * 1e6) if secs > 0 else 0.0
    
    return d, talker_tx_mbps, listener_rx_mbps


def parse_latency_samples_from_logs(docker_cli, *, only_service="listener"):
    """Parse BENCH_LATENCY_MS samples from container logs."""
    cache = {}
    out = {}
    for c in docker_cli.compose.ps():
        svc = service_of(docker_cli, c, cache)
        if only_service and svc != only_service:
            continue
        cname = getattr(c, "container_name", "") or c.id
        try:
            txt = docker_cli.container.logs(c.id)
        except Exception as e:
            log(f"  warning: failed to read logs for {svc}:{cname}: {e}")
            continue
        out[cname] = [float(m.group(1)) for m in LAT_RE.finditer(txt or "")]
    return out


def hop_histogram():
    """Get distribution of listener hop counts from routing state."""
    hist = defaultdict(int)
    try:
        r = rds()
        keys = r.keys("*-routing")
        states = []
        for k in keys:
            raw = r.get(k)
            if not raw:
                continue
            try:
                st = json.loads(raw)
            except Exception:
                continue
            states.append(st)
        r.close()
    except Exception as e:
        log(f"  hop_histogram warning: {e}")
        return dict(hist)

    for st in states:
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
        
        # BFS from root
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
    """Count currently connected listeners from routing state."""
    connected = set()
    try:
        r = rds()
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
    except Exception as e:
        log(f"  connected_listener_count warning: {e}")
    return len(connected)


# Statistics helpers
def mean_std(vals):
    """Compute mean and standard deviation."""
    if not vals:
        return 0.0, 0.0
    m = sum(vals) / len(vals)
    v = sum((x - m) ** 2 for x in vals) / len(vals)
    return m, math.sqrt(max(0.0, v))


def percentile(vals, p):
    """Compute percentile (p in 0-100)."""
    if not vals:
        return 0.0
    s = sorted(vals)
    k = int(round((p / 100.0) * (len(s) - 1)))
    k = max(0, min(len(s) - 1, k))
    return float(s[k])
