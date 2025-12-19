#!/usr/bin/env python3
"""Latency probe listener for benchmarking.

Receives timestamped messages and computes end-to-end latency.
Uses wall-clock time (time.time_ns) for cross-container synchronization.
"""
import os
import time

import rclpy
from rclpy.node import Node
from std_msgs.msg import String


class LatencyListener(Node):
    def __init__(self):
        super().__init__("bench_latency_listener")
        topic = os.environ.get("BENCH_LATENCY_TOPIC", "/chatter")
        self.samples = []
        self.max_samples = int(os.environ.get("BENCH_LATENCY_MAX_SAMPLES", "5000"))
        self.sub = self.create_subscription(String, topic, self._cb, 10)
        self.last_print = time.time()
        self.print_every = float(os.environ.get("BENCH_LATENCY_PRINT_EVERY_SECS", "5.0"))

    def _cb(self, msg: String):
        # Use wall-clock time immediately on receive
        recv_ns = time.time_ns()
        
        data = msg.data or ""
        prefix = "BENCH_LATENCY_SENT_NS="
        if not data.startswith(prefix):
            return

        try:
            sent_ns = int(data[len(prefix):].strip())
        except Exception:
            return
        
        lat_ms = max(0.0, (recv_ns - sent_ns) / 1e6)

        # Emit parseable sample line (scraped by benchmark scripts)
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
            p50 = s[len(s) // 2]
            p95 = s[int(0.95 * (len(s) - 1))]
            p99 = s[int(0.99 * (len(s) - 1))]
            mean = sum(s) / len(s)
            print(
                f"BENCH_LATENCY_SUMMARY count={len(s)} mean={mean:.1f}ms p50={p50:.1f}ms p95={p95:.1f}ms p99={p99:.1f}ms max={s[-1]:.1f}ms",
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
