#!/usr/bin/env python3
"""Latency probe publisher for benchmarking.

Publishes timestamped messages at configurable frequency.
Uses wall-clock time (time.time_ns) for cross-container synchronization.
Supports variable payload size via BENCH_PAYLOAD_BYTES.
"""
import os
import time

import rclpy
from rclpy.node import Node
from std_msgs.msg import String


class LatencyTalker(Node):
    def __init__(self):
        super().__init__("bench_latency_talker")
        topic = os.environ.get("BENCH_LATENCY_TOPIC", "/chatter")
        hz = float(os.environ.get("BENCH_LATENCY_HZ", "20.0"))
        payload_bytes = int(os.environ.get("BENCH_PAYLOAD_BYTES", "0"))
        self.print_every = float(os.environ.get("BENCH_LATENCY_PRINT_EVERY_SECS", "5.0"))
        self._last_print = time.time()
        self._sent = 0
        self._bytes_sent = 0
        # Pre-compute padding if payload size specified
        self._padding = "X" * max(0, payload_bytes - 40) if payload_bytes > 0 else ""
        self.pub = self.create_publisher(String, topic, 10)
        period = 1.0 / max(hz, 0.1)
        self.timer = self.create_timer(period, self._tick)
        self.get_logger().info(f"LatencyTalker: hz={hz} payload={payload_bytes}B")

    def _tick(self):
        sent_ns = time.time_ns()
        msg = String()
        if self._padding:
            msg.data = f"BENCH_LATENCY_SENT_NS={sent_ns}|{self._padding}"
        else:
            msg.data = f"BENCH_LATENCY_SENT_NS={sent_ns}"
        self.pub.publish(msg)
        self._sent += 1
        self._bytes_sent += len(msg.data)

        t = time.time()
        if t - self._last_print >= self.print_every:
            elapsed = t - self._last_print + 0.001
            mbps = (self._bytes_sent * 8 / 1e6) / elapsed
            print(f"BENCH_LATENCY_SENT_COUNT={self._sent} mbps={mbps:.2f}", flush=True)
            self._last_print = t
            self._bytes_sent = 0


def main():
    rclpy.init()
    node = LatencyTalker()
    try:
        rclpy.spin(node)
    finally:
        node.destroy_node()
        rclpy.shutdown()


if __name__ == "__main__":
    main()
