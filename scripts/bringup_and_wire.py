#!/usr/bin/env python3
"""
Minimal bring-up for the default demo:
- starts docker-compose,
- discovers GDP names for talker -> proxy -> listener,
- seeds Redis links,
- waits for listener output, then tears everything down.
"""

import hashlib
import re
import sys
import time
from pathlib import Path

import redis
import yaml
from python_on_whales import DockerClient

LOG_GDP_REGEX = re.compile(r"My GDP name is:\s*([0-9a-fA-F]{8})")
LOG_TOPIC_GDP_REGEX = re.compile(r"Topic /chatter has connection topic:\s*([0-9a-fA-F]{8})")

COMPOSE_FILE = Path("docker-compose.yaml")
PROJECT_NAME = "fogros2-sgc-lite"
REDIS_URL = "redis://localhost:8002"
TOPIC_NAME = "/chatter"
TOPIC_TYPE = "sensor_msgs/msg/CompressedImage"
CERT_PATH = Path("scripts/crypto/test_cert/test_cert-private.pem")
TALKER_SERVICE = "talker"
LISTENER_SERVICE = "listener"
VERIFY_TIMEOUT = 1200
RESET_REDIS = False
KEEP_UP = False


def compute_topic_gdp(topic_name: str, topic_type: str, cert_path: Path) -> str:
    cert_bytes = cert_path.read_bytes()
    digest = hashlib.sha256()
    digest.update(topic_name.encode())
    digest.update(topic_type.encode())
    digest.update(cert_bytes)
    raw = digest.digest()[:4]
    return "".join(f"{b:02x}" for b in raw)


def wait_for_gdp(docker: DockerClient, service: str, timeout: int = 90) -> str:
    deadline = time.time() + timeout
    last_logs = ""
    while time.time() < deadline:
        last_logs = docker.compose.logs(services=[service])
        match = LOG_GDP_REGEX.search(last_logs)
        if match:
            return match.group(1)
        time.sleep(3)
    raise RuntimeError(f"Could not find GDP name in logs for {service}.\n{last_logs[-400:]}")


def wait_for_topic_gdp(docker: DockerClient, service: str, timeout: int = 120):
    deadline = time.time() + timeout
    while time.time() < deadline:
        logs = docker.compose.logs(services=[service])
        match = LOG_TOPIC_GDP_REGEX.search(logs)
        if match:
            return match.group(1)
        time.sleep(3)
    return None


def ensure_list_member(r: redis.Redis, key: str, value: str) -> None:
    if value not in r.lrange(key, 0, -1):
        r.lpush(key, value)


def parse_compose_services(compose_file: Path):
    try:
        parsed = yaml.safe_load(compose_file.read_text()) or {}
        return parsed.get("services", {}) or {}
    except Exception as exc:
        sys.stderr.write(f"Warning: failed to parse compose file {compose_file}: {exc}\n")
        return {}


def detect_proxies(services: dict) -> list[str]:
    proxies: list[str] = []
    for name, svc in services.items():
        env = svc.get("environment", {})
        cfg = None
        if isinstance(env, dict):
            cfg = env.get("SGC_CONFIG")
        elif isinstance(env, list):
            for entry in env:
                if isinstance(entry, str) and entry.startswith("SGC_CONFIG="):
                    cfg = entry.split("=", 1)[1]
                    break
        cfg_str = (cfg or "").lower()
        if "proxy" in name.lower() or "proxy" in cfg_str:
            proxies.append(name)
    return proxies


services = parse_compose_services(COMPOSE_FILE)
proxies = detect_proxies(services)
if not proxies and "proxy" in services:
    proxies = ["proxy"]

if proxies:
    print(f"Using proxy services: {proxies}")
else:
    print("No proxy detected; wiring talker -> listener directly")

docker = DockerClient(
    compose_files=[str(COMPOSE_FILE)],
    compose_project_name=PROJECT_NAME,
)

started_compose = False

try:
    print("Starting docker-compose (detached)...")
    docker.compose.down()
    docker.compose.up(detach=True, build=True)
    started_compose = True

    path_services = [TALKER_SERVICE] + proxies + [LISTENER_SERVICE]
    seen = set()
    ordered_services = []
    for svc in path_services:
        if svc not in seen:
            ordered_services.append(svc)
            seen.add(svc)

    print("Waiting for GDP names from logs...")
    gdp_names = {}
    for svc in ordered_services:
        gdp_names[svc] = wait_for_gdp(docker, svc)
    print("GDP map:", gdp_names)

    topic_gdp = wait_for_topic_gdp(docker, TALKER_SERVICE)
    if not topic_gdp:
        topic_gdp = compute_topic_gdp(TOPIC_NAME, TOPIC_TYPE, CERT_PATH)
        print(f"Topic GDP not found in logs; computed {topic_gdp}")
    else:
        print(f"Found topic GDP in logs: {topic_gdp}")

    r = redis.Redis.from_url(REDIS_URL, decode_responses=True)

    connections_key = f"{topic_gdp}-connections"
    meta_key = f"{topic_gdp}-meta"

    # Reset connection state to avoid stale/incorrect chains.
    r.delete(connections_key)
    if RESET_REDIS:
        r.delete(meta_key)
        for proxy in proxies:
            r.delete(f"{gdp_names[proxy]}-proxy-topics")

    r.hset(meta_key, mapping={"topic_name": TOPIC_NAME, "topic_type": TOPIC_TYPE})

    for proxy in proxies:
        proxy_topics_key = f"{gdp_names[proxy]}-proxy-topics"
        ensure_list_member(r, proxy_topics_key, topic_gdp)
        print(f"  {proxy_topics_key} += {topic_gdp}")

    path_gdps = [gdp_names[s] for s in path_services]
    connections = [f"{left}-{right}" for left, right in zip(path_gdps, path_gdps[1:])]
    for conn in connections:
        r.rpush(connections_key, conn)
        print(f"  {connections_key} -> {conn}")

    print("Redis wiring complete. Waiting for listener output...")

    deadline = time.time() + VERIFY_TIMEOUT
    last_logs = ""
    while time.time() < deadline:
        logs = docker.compose.logs(services=[LISTENER_SERVICE], tail=200)
        last_logs = logs
        time.sleep(3)
    else:
        sys.stderr.write(
            f"Listener {LISTENER_SERVICE} did not receive messages within {VERIFY_TIMEOUT}s\n"
        )
        for svc in path_services:
            print(f"--- logs for {svc} ---")
            try:
                print(docker.compose.logs(services=[svc], tail=200).strip())
            except Exception as exc:
                print(f"[warn] failed to fetch logs for {svc}: {exc}")
        sys.exit(1)
finally:
    if started_compose and not KEEP_UP:
        print("Stopping docker-compose...")
        docker.compose.down(remove_orphans=True)
