#!/usr/bin/env python3
"""
Spin up the compose stack with scaled listeners/proxies and verify all listeners
log an "I heard" line within a timeout. Uses python-on-whales.
"""
import time
import sys
from pathlib import Path

from python_on_whales import DockerClient
import redis

COMPOSE_FILE = Path(__file__).parent.parent / "docker-compose.yaml"
PROJECT_NAME = "fogros2-sgc-lite"
TIMEOUT = 20 # seconds
LISTENER_COUNT = 6
PROXY_COUNT = 1
LOG_PATTERN = "I heard"
REDIS_HOST = "localhost"
REDIS_PORT = 8002
MAX_CONNECTIONS_PER_NODE = 3
MAX_SUBSCRIBERS_PER_PUBLISHER = 3
  

def check_connection_fanout():
    """Ensure no publisher fans out to >=4 subscribers in the connections list."""
    client = redis.Redis(host=REDIS_HOST, port=REDIS_PORT, decode_responses=True)
    try:
        keys = client.keys("*-connections")
    except Exception as exc:
        print(f"[fail] redis key scan failed: {exc}")
        return False

    if not keys:
        print("[warn] no connection tables found in redis; skipping fanout check")
        return True

    fanout = {}
    for key in keys:
        try:
            entries = client.lrange(key, 0, -1)
        except Exception as exc:
            print(f"[fail] redis lrange failed for {key}: {exc}")
            return False

        for conn in entries:
            if "-" not in conn:
                continue
            src, dst = conn.split("-", 1)
            if not src or not dst:
                continue
            fanout.setdefault(src, set()).add(dst)

    offenders = {src: len(dsts) for src, dsts in fanout.items() if len(dsts) > MAX_SUBSCRIBERS_PER_PUBLISHER}
    if offenders:
        print(f"[fail] publisher fanout exceeds {MAX_SUBSCRIBERS_PER_PUBLISHER}: {offenders}")
        return False

    print(f"[pass] publisher fanout within limit ({MAX_SUBSCRIBERS_PER_PUBLISHER})")
    return True


def main():
    docker = DockerClient(
        compose_files=[COMPOSE_FILE],
        compose_project_name=PROJECT_NAME,
    )
    print("Starting compose with scales...")
    docker.compose.up(
        detach=True,
        build=True,
        scales={"listener": LISTENER_COUNT, "proxy": PROXY_COUNT, "redisinsight": 0},
    )

    try:
        deadline = time.time() + TIMEOUT
        # Fetch the actual container instances for the listener service
        listeners = docker.compose.ps(services=["listener"])
        remaining = {container.name for container in listeners}
        print(f"Waiting for listeners to emit '{LOG_PATTERN}'... targets: {remaining}")

        while time.time() < deadline and remaining:
            time.sleep(1)
            for svc in list(remaining):
                logs = docker.container.logs(svc, tail=200)
                if LOG_PATTERN in str(logs):
                    print(f"[ok] {svc} logged pattern")
                    remaining.remove(svc)

        if remaining:
            print(f"[fail] Timed out waiting for: {remaining}")
            sys.exit(1)
        print("[pass] All listeners received messages.")

        if not check_connection_fanout():
            sys.exit(1)
    finally:
        print("Stopping compose...")
        docker.compose.down()


if __name__ == "__main__":
    main()
