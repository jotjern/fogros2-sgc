import rclpy
from rclpy.node import Node
from std_msgs.msg import String
import time

# Benchmark parameters
MESSAGE_SIZE_BYTES = 10000    # Payload size per message
MESSAGE_RATE_HZ = 10         # Messages per second
# Expected bandwidth: MESSAGE_SIZE_BYTES * MESSAGE_RATE_HZ

class ThroughputPublisher(Node):
    def __init__(self):
        super().__init__('throughput_publisher')
        self.publisher_ = self.create_publisher(String, 'chatter', 10)
        self.payload = "1" * MESSAGE_SIZE_BYTES
        self.interval = 1.0 / MESSAGE_RATE_HZ
        self.get_logger().info(
            f'Publishing {MESSAGE_SIZE_BYTES} bytes at {MESSAGE_RATE_HZ} Hz '
            f'(~{MESSAGE_SIZE_BYTES * MESSAGE_RATE_HZ / 1_000_000:.1f} MB/s)'
        )

    def publish_message(self):
        msg = String()
        msg.data = self.payload
        self.publisher_.publish(msg)


def main(args=None):
    rclpy.init(args=args)
    node = ThroughputPublisher()
    next_time = time.time()
    while rclpy.ok():
        node.publish_message()
        next_time += node.interval
        sleep_duration = next_time - time.time()
        if sleep_duration > 0:
            time.sleep(sleep_duration)
    node.destroy_node()
    rclpy.shutdown()


if __name__ == '__main__':
    main()
