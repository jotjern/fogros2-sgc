#!/usr/bin/env python3
import json
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


def choose_proxies(num_listeners, fanout):
    f = int(fanout)
    if f <= 0:
        f = 1
    return max(1, ((int(num_listeners) + f - 1) // f) * 2)


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
    # join latency = bench_first_msg_ms - bench_join_attempt_ms (both set by rust hooks)
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
    out = {}
    # Docker can occasionally return transient EOFs for `container stats` while
    # containers are being (re)created. Treat that as retryable; otherwise a
    # single hiccup kills the whole benchmark run.
    last_err = None
    stats = None
    ps = []
    cache = {}
    name_to_service = {}
    for attempt in range(5):
        ps = docker_cli.compose.ps()
        if not ps:
            return out
        cache = {}
        name_to_service = {
            (getattr(c, "container_name", None) or getattr(c, "name", None) or c.id): service_of(docker_cli, c, cache)
            for c in ps
        }
        try:
            stats = docker_cli.container.stats([c.id for c in ps])
            last_err = None
            break
        except Exception as e:
            last_err = e
            time.sleep(0.3 * (attempt + 1))
            continue
    if stats is None:
        raise RuntimeError(f"failed to read docker stats after retries: {last_err}")

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


def measure_net(docker_cli, measure_secs):
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
            raise RuntimeError(f"failed to read logs for {svc}:{cname}: {e}")
        out[cname] = [float(m.group(1)) for m in LAT_RE.finditer(txt or "")]
    return out


def hop_histogram():
    # aggregate across all topics in redis
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
        raise RuntimeError(f"failed to read routing state for hops: {e}")

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
        raise RuntimeError(f"failed to count connected listeners: {e}")
    return len(connected)

