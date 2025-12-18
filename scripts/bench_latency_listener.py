#!/usr/bin/env python3
import os
import time

import rclpy
from builtin_interfaces.msg import Time as TimeMsg
from rclpy.node import Node


class LatencyListener(Node):
    def __init__(self):
        super().__init__("bench_latency_listener")
        topic = os.environ.get("BENCH_LATENCY_TOPIC", "/bench_latency")
        self.samples = []
        self.max_samples = int(os.environ.get("BENCH_LATENCY_MAX_SAMPLES", "2000"))
        self.sub = self.create_subscription(TimeMsg, topic, self._cb, 10)
        self.last_print = time.time()
        self.print_every = float(os.environ.get("BENCH_LATENCY_PRINT_EVERY_SECS", "1.0"))

    def _cb(self, msg: TimeMsg):
        now = self.get_clock().now()
        sent_ns = int(msg.sec) * 1_000_000_000 + int(msg.nanosec)
        recv_ns = int(now.nanoseconds)
        lat_ms = max(0.0, (recv_ns - sent_ns) / 1e6)

        # Emit a parseable sample line.
        # The benchmark script scrapes these from container logs.
        print(f"BENCH_LATENCY_MS={lat_ms:.3f}", flush=True)

        self.samples.append(lat_ms)
        if len(self.samples) > self.max_samples:
            self.samples.pop(0)

        t = time.time()
        if t - self.last_print >= self.print_every:
            self.last_print = t
            s = list(self.samples)
            if not s:
                return
            s.sort()
            p95 = s[int(0.95 * (len(s) - 1))]
            mean = sum(s) / len(s)
            print(
                f"BENCH_LATENCY_SUMMARY count={len(s)} mean_ms={mean:.3f} p95_ms={p95:.3f} max_ms={s[-1]:.3f}",
                flush=True,
            )


def main():
    rclpy.init()
    node = LatencyListener()
    try:
        rclpy.spin(node)
    finally:
        node.destroy_node()
        rclpy.shutdown()


if __name__ == "__main__":
    main()

