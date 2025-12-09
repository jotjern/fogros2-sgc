"""
Measure outbound traffic of the talker container and verify listener inbound rates
using python_on_whales with the same DockerClient pattern as other scripts.
All parameters are hard-coded below for simplicity.
"""

import sys
import time
from pathlib import Path
from typing import Dict, List, Tuple

from python_on_whales import DockerClient

# ---- Hard-coded configuration ----
COMPOSE_FILE = Path(__file__).resolve().parent.parent / "docker-compose.yaml"
PROJECT_NAME = "fogros2-sgc-lite"

TALKER_SERVICE = "talker"
LISTENER_SERVICE = "listener"
LISTENER_COUNT = 1  # If scaled (listener-1, listener-2...), set >1 to check each
LISTENER_NAMES: List[str] = []  # If non-empty, overrides LISTENER_COUNT
TOLERANCE = 0.10  # 10% allowed spread between listener inbound rates
STABILIZE_SECONDS = 5.0
SAMPLE_DELAY = 1.0  # seconds for the byte diff interval
WAIT_RX_TIMEOUT = 10.0  # seconds to wait for non-zero inbound on all listeners
# ----------------------------------

docker = DockerClient(
    compose_files=[str(COMPOSE_FILE)],
    compose_project_name=PROJECT_NAME,
)


def read_net_bytes(container: str) -> Tuple[int, int]:
    """Return (rx_bytes, tx_bytes) summed across interfaces for a container."""
    stats = docker.container.stats(container)
    networks = getattr(stats, "networks", {}) or {}
    rx = sum(iface.rx_bytes for iface in networks.values())
    tx = sum(iface.tx_bytes for iface in networks.values())
    return rx, tx


def rate_bps(container: str, delay: float = 1.0, direction: str = "tx") -> float:
    """Bytes per second for the selected direction over a short interval."""
    rx1, tx1 = read_net_bytes(container)
    time.sleep(delay)
    rx2, tx2 = read_net_bytes(container)
    if direction == "tx":
        return (tx2 - tx1) / delay
    return (rx2 - rx1) / delay


def resolve_service_containers(service: str) -> List[str]:
    """Return container names for a given compose service."""
    containers = docker.compose.ps(services=[service])
    return sorted([c.name for c in containers])


def build_listener_list() -> List[str]:
    if LISTENER_NAMES:
        return LISTENER_NAMES
    names = resolve_service_containers(LISTENER_SERVICE)
    if not names:
        raise RuntimeError(f"No containers found for service '{LISTENER_SERVICE}'")
    if LISTENER_COUNT and LISTENER_COUNT < len(names):
        names = names[:LISTENER_COUNT]
    return names


def get_single_talker_name() -> str:
    names = resolve_service_containers(TALKER_SERVICE)
    if not names:
        raise RuntimeError(f"No containers found for service '{TALKER_SERVICE}'")
    return names[0]


def main():
    listeners = build_listener_list()
    talker_name = get_single_talker_name()

    print(f"Waiting {STABILIZE_SECONDS:.1f}s for streams to stabilize...")
    time.sleep(STABILIZE_SECONDS)

    def measure_inbound() -> Dict[str, float]:
        rates: Dict[str, float] = {}
        for name in listeners:
            try:
                rates[name] = rate_bps(name, delay=SAMPLE_DELAY, direction="rx")
            except Exception as exc:  # pylint: disable=broad-except
                raise RuntimeError(f"Failed to read listener {name} stats: {exc}") from exc
        return rates

    # Wait until all listeners see non-zero inbound, up to WAIT_RX_TIMEOUT.
    deadline = time.time() + WAIT_RX_TIMEOUT
    inbound_rates: Dict[str, float] = {}
    while time.time() < deadline:
        inbound_rates = measure_inbound()
        if inbound_rates and all(val > 0 for val in inbound_rates.values()):
            break
        time.sleep(1)

    if not inbound_rates:
        print("No listeners specified; nothing to check.")
        return
    if any(val <= 0 for val in inbound_rates.values()):
        print(
            f"Listeners have zero inbound after waiting {WAIT_RX_TIMEOUT}s: "
            f"{ {k: v for k, v in inbound_rates.items()} }"
        )
        sys.exit(2)

    try:
        talker_tx = rate_bps(talker_name, delay=SAMPLE_DELAY, direction="tx")
    except Exception as exc:  # pylint: disable=broad-except
        print(f"Failed to read talker stats: {exc}")
        sys.exit(1)
docker compose up -d --scale web=3

    min_rx = min(inbound_rates.values())
    max_rx = max(inbound_rates.values())
    ok_spread = max_rx > 0 and (max_rx - min_rx) <= TOLERANCE * max_rx

    print(f"Talker outbound: {talker_tx/1024:.2f} KB/s")
    for name, val in inbound_rates.items():
        print(f"Listener {name} inbound: {val/1024:.2f} KB/s")

    if ok_spread:
        print(
            f"All listeners within {TOLERANCE*100:.1f}% of each other "
            f"(min={min_rx/1024:.2f} KB/s, max={max_rx/1024:.2f} KB/s)."
        )
    else:
        print(
            f"Listeners differ by more than {TOLERANCE*100:.1f}% "
            f"(min={min_rx/1024:.2f} KB/s, max={max_rx/1024:.2f} KB/s)."
        )
        sys.exit(2)


if __name__ == "__main__":
    main()
