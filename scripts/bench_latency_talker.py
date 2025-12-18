#!/usr/bin/env python3
import os
import time

import rclpy
from builtin_interfaces.msg import Time as TimeMsg
from rclpy.node import Node


class LatencyTalker(Node):
    def __init__(self):
        super().__init__("bench_latency_talker")
        topic = os.environ.get("BENCH_LATENCY_TOPIC", "/bench_latency")
        hz = float(os.environ.get("BENCH_LATENCY_HZ", "20.0"))
        self.print_every = float(os.environ.get("BENCH_LATENCY_PRINT_EVERY_SECS", "1.0"))
        self._last_print = time.time()
        self._sent = 0
        self.pub = self.create_publisher(TimeMsg, topic, 10)
        period = 1.0 / max(hz, 0.1)
        self.timer = self.create_timer(period, self._tick)

    def _tick(self):
        now = self.get_clock().now().to_msg()
        msg = TimeMsg(sec=int(now.sec), nanosec=int(now.nanosec))
        self.pub.publish(msg)
        self._sent += 1

        t = time.time()
        if t - self._last_print >= self.print_every:
            self._last_print = t
            print(f"BENCH_LATENCY_SENT_COUNT={self._sent}", flush=True)


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

