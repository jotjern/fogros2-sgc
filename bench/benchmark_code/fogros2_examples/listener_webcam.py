import os
import threading
import time
from http.server import BaseHTTPRequestHandler, HTTPServer
from typing import Optional

import rclpy
from rclpy.node import Node
from sensor_msgs.msg import CompressedImage


LATEST_IMAGE: Optional[bytes] = None
LATEST_TIMESTAMP: Optional[str] = None
LOCK = threading.Lock()
DEFAULT_PORT = int(os.environ.get("LISTENER_HTTP_PORT", "8081"))


class ImageHandler(BaseHTTPRequestHandler):
    def do_GET(self):
        if self.path.startswith("/latest.jpg"):
            with LOCK:
                img = LATEST_IMAGE
            if not img:
                self.send_response(503)
                self.end_headers()
                self.wfile.write(b"No image received yet")
                return
            self.send_response(200)
            self.send_header("Content-Type", "image/jpeg")
            self.send_header("Cache-Control", "no-store")
            self.end_headers()
            self.wfile.write(img)
        else:
            self.send_response(200)
            self.send_header("Content-Type", "text/html")
            self.end_headers()
            ts = LATEST_TIMESTAMP or "waiting for first frame..."
            self.wfile.write(
                f"<html><body><h2>Latest frame</h2><p>{ts}</p><img src=\"/latest.jpg?ts={time.time()}\" style=\"max-width:100%;\"></body></html>".encode()
            )

    def log_message(self, format, *args):
        return


def start_http_server(port: int):
    server = HTTPServer(("0.0.0.0", port), ImageHandler)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    return server


class MinimalSubscriber(Node):
    def __init__(self):
        super().__init__("minimal_subscriber")
        self.start_time = time.time()
        self.received_msgs = 0
        self.subscription = self.create_subscription(
            CompressedImage, "/chatter", self.callback, 10
        )
        self.http_server = start_http_server(DEFAULT_PORT)
        self.get_logger().info(f"HTTP server started on 0.0.0.0:{DEFAULT_PORT}")

    def callback(self, msg: CompressedImage):
        global LATEST_IMAGE, LATEST_TIMESTAMP
        self.received_msgs += 1
        with LOCK:
            LATEST_IMAGE = bytes(msg.data)
            LATEST_TIMESTAMP = time.strftime(
                "%Y-%m-%d %H:%M:%S", time.gmtime()
            ) + " UTC"

        if self.received_msgs % 50 == 0:
            elapsed_time = time.time() - self.start_time
            msg_throughput = self.received_msgs / elapsed_time
            self.get_logger().info(
                f"Received {self.received_msgs} images, throughput: {msg_throughput:.2f} msg/s"
            )


def main(args=None):
    rclpy.init(args=args)
    # Randomized startup delay (0-10s) to stagger listener connections
    import random
    delay = random.uniform(0, 20)
    time.sleep(delay)

    minimal_subscriber = MinimalSubscriber()
    try:
        rclpy.spin(minimal_subscriber)
    finally:
        minimal_subscriber.destroy_node()
        rclpy.shutdown()


if __name__ == "__main__":
    main()
