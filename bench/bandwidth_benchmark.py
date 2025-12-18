#!/usr/bin/env python3
import argparse
import time
import json
import os
from datetime import datetime
import redis
from python_on_whales import DockerClient

REDIS_HOST = "localhost"
REDIS_PORT = 8002
PROJECT_DIR = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
DEFAULT_TIMEOUT_SECS = 180
DEFAULT_MEASURE_SECS = 30
DEFAULT_OUT_JSON = None


def default_out_path():
    ts = datetime.now().strftime("%Y%m%d_%H%M%S")
    return os.path.join(PROJECT_DIR, "bench", "results", f"bandwidth_results_{ts}.json")

docker = DockerClient(compose_files=[os.path.join(PROJECT_DIR, "docker-compose.yaml")])

def get_redis():
    return redis.Redis(host=REDIS_HOST, port=REDIS_PORT, decode_responses=True)

def get_connected_listeners(r):
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
        except:
            pass
    return connected

def wait_for_listeners(expected, timeout_secs: int):
    start = time.time()
    while time.time() - start < timeout_secs:
        try:
            r = get_redis()
            connected = get_connected_listeners(r)
            r.close()
            print(f"  {len(connected)}/{expected} listeners connected ({int(time.time()-start)}s)")
            if len(connected) >= expected:
                return True, connected
        except:
            print(f"  waiting for redis... ({int(time.time()-start)}s)")
        time.sleep(3)
    return False, set()

def get_service_name(container_name):
    parts = container_name.split("-")
    if len(parts) >= 2:
        return parts[-2]
    return container_name

def get_net_stats():
    stats = {}
    # Only measure containers from *this* docker-compose project.
    containers = docker.compose.ps()
    if not containers:
        return stats
    stats_list = docker.container.stats([c.id for c in containers])
    for s in stats_list:
        stats[s.container_name] = {
            "service": get_service_name(s.container_name),
            "rx": s.net_download,
            "tx": s.net_upload
        }
    return stats

def choose_num_proxies(proxy_mode: str, num_listeners: int, fanout_factor: int) -> int:
    if proxy_mode == "none":
        return 0
    if proxy_mode == "one":
        return 1
    f = int(fanout_factor) if int(fanout_factor) > 0 else 1
    return max(1, ((num_listeners + f - 1) // f) * 2)


def run_benchmark(
    num_listeners,
    fanout_factor,
    measure_secs: int,
    timeout_secs: int,
    proxy_mode: str,
    proxy_count_override: int | None,
):
    print(f"\n{'='*50}\nBenchmark: fanout={fanout_factor} listeners={num_listeners}\n{'='*50}")
    
    num_proxies = proxy_count_override if proxy_count_override is not None else choose_num_proxies(proxy_mode, num_listeners, fanout_factor)
    success, connected = False, set()
    for attempt in range(1, 4):
        # Ensure docker-compose variable interpolation + container env pick up the desired fanout.
        os.environ["FANOUT_FACTOR"] = str(fanout_factor)

        docker.compose.down(volumes=True, remove_orphans=True)
        time.sleep(2)

        docker.compose.up(detach=True, scales={"listener": num_listeners, "proxy": num_proxies})
        success, connected = wait_for_listeners(num_listeners, timeout_secs)
        if success:
            break
        print(f"  FAILED (attempt {attempt}/3): only {len(connected)} listeners connected")
        if attempt < 3:
            print("  Retrying...")
            docker.compose.down(volumes=True, remove_orphans=True)
            time.sleep(3)

    if not success:
        docker.compose.down(volumes=True, remove_orphans=True)
        return {"fanout": fanout_factor, "num_listeners": num_listeners, "success": False, "attempts": 3}
    
    print(f"  All listeners connected, measuring bandwidth for {measure_secs}s...")
    stats1 = get_net_stats()
    time.sleep(measure_secs)
    stats2 = get_net_stats()
    
    results = {"fanout": fanout_factor, "num_listeners": num_listeners, "success": True, "containers": {}}
    total_rx, total_tx = 0, 0
    
    for name in stats2:
        if name in stats1:
            rx = stats2[name]["rx"] - stats1[name]["rx"]
            tx = stats2[name]["tx"] - stats1[name]["tx"]
            results["containers"][name] = {"service": stats2[name].get("service"), "rx": rx, "tx": tx}
            total_rx += rx
            total_tx += tx
    
    results["total_rx"] = total_rx
    results["total_tx"] = total_tx
    results["total"] = total_rx + total_tx
    
    print(f"  Total: RX={total_rx/1e6:.2f}MB TX={total_tx/1e6:.2f}MB")
    
    docker.compose.down(volumes=True)
    time.sleep(3)
    return results

if __name__ == "__main__":
    ap = argparse.ArgumentParser()
    ap.add_argument("--listeners", default="1,5,10", help="Comma-separated listener counts (e.g. 1,5,10)")
    ap.add_argument("--fanouts", default="3,1000", help="Comma-separated fanout factors (e.g. 3,1000)")
    ap.add_argument("--measure-secs", type=int, default=DEFAULT_MEASURE_SECS)
    ap.add_argument("--timeout-secs", type=int, default=DEFAULT_TIMEOUT_SECS)
    ap.add_argument("--proxy-mode", choices=["auto", "one", "none"], default="auto")
    ap.add_argument("--proxies", type=int, default=None, help="Override number of proxy containers to scale to (e.g. 5).")
    ap.add_argument("--out", default=DEFAULT_OUT_JSON, help="Output JSON path.")
    args = ap.parse_args()

    listener_counts = [int(x.strip()) for x in args.listeners.split(",") if x.strip()]
    fanout_passes = [int(x.strip()) for x in args.fanouts.split(",") if x.strip()]

    output_dir = os.path.join(PROJECT_DIR, "bench", "results")
    os.makedirs(output_dir, exist_ok=True)
    
    ts = datetime.now().strftime("%Y%m%d_%H%M%S")
    out_path = args.out or default_out_path()

    # Overwrite the same JSON each run, but keep it incrementally updated
    # after each benchmark so partial results are still usable if interrupted.
    results_all = []
    header = {
        "schema": "fogros2-sgc.bench.bandwidth_results.v1",
        "created_at": ts,
        "measure_secs": args.measure_secs,
        "timeout_secs": args.timeout_secs,
        "proxy_mode": args.proxy_mode,
        "proxy_count": args.proxies,
        "fanouts": fanout_passes,
        "listeners": listener_counts,
    }

    os.makedirs(os.path.dirname(out_path) or ".", exist_ok=True)
    with open(out_path, "w") as f:
        json.dump({"meta": header, "runs": results_all}, f, indent=2)

    for fanout in fanout_passes:
        for n in listener_counts:
            try:
                r = run_benchmark(
                    n,
                    fanout,
                    measure_secs=args.measure_secs,
                    timeout_secs=args.timeout_secs,
                    proxy_mode=args.proxy_mode,
                    proxy_count_override=args.proxies,
                )
                results_all.append(r)
            except Exception as e:
                print(f"  ERROR: {e}")
                results_all.append({"fanout": fanout, "num_listeners": n, "success": False, "error": str(e)})

            # rewrite the same file each time
            with open(out_path, "w") as f:
                json.dump({"meta": header, "runs": results_all}, f, indent=2)

    print(f"\nDone. Results: {out_path}")
