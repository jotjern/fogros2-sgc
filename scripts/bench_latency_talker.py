#!/usr/bin/env python3
import os
import time

import rclpy
from rclpy.node import Node
from std_msgs.msg import String


class LatencyTalker(Node):
    def __init__(self):
        super().__init__("bench_latency_talker")
        # Publish *latency probe* messages onto /chatter so we don't introduce a second topic.
        # We tag messages so the listener can ignore normal chatter from demo_nodes_py talker.
        topic = os.environ.get("BENCH_LATENCY_TOPIC", "/chatter")
        hz = float(os.environ.get("BENCH_LATENCY_HZ", "20.0"))
        self.print_every = float(os.environ.get("BENCH_LATENCY_PRINT_EVERY_SECS", "1.0"))
        self._last_print = time.time()
        self._sent = 0
        self.pub = self.create_publisher(String, topic, 10)
        period = 1.0 / max(hz, 0.1)
        self.timer = self.create_timer(period, self._tick)

    def _tick(self):
        # Note: this measures "latency + clock skew". In our docker benchmark setup,
        # talker/listener are on the same host so skew should be small/stable.
        sent_ns = int(self.get_clock().now().nanoseconds)
        msg = String()
        msg.data = f"BENCH_LATENCY_SENT_NS={sent_ns}"
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

