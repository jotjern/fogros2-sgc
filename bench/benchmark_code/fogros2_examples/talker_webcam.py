import io
from datetime import datetime
from urllib import request

import rclpy
from rclpy.node import Node
from sensor_msgs.msg import CompressedImage
from PIL import Image, ImageDraw, ImageFont


IMAGE_URL = "https://www.ocf.berkeley.edu/~thelawrence/images/newview.jpg"


class MinimalPublisher(Node):
    def __init__(self):
        super().__init__("minimal_publisher")
        self.publisher_ = self.create_publisher(CompressedImage, "/chatter", 10)
        self.base_image = self._fetch_image()
        self.timer = self.create_timer(1.0, self.timer_callback)

    def _fetch_image(self) -> Image.Image:
        try:
            self.get_logger().info(f"Fetching image from {IMAGE_URL}")
            with request.urlopen(IMAGE_URL, timeout=5) as resp:
                data = resp.read()
            img = Image.open(io.BytesIO(data)).convert("RGB")
            self.get_logger().info("Image fetched successfully")
            return img
        except Exception as exc:
            self.get_logger().error(f"Failed to fetch image: {exc}")
            # fallback: tiny black image
            return Image.new("RGB", (320, 240), color="black")

    def _overlay_timestamp(self, img: Image.Image) -> CompressedImage:
        # Work on a copy to avoid mutating base_image
        stamped = img.copy()
        draw = ImageDraw.Draw(stamped)
        timestamp = datetime.utcnow().strftime("%Y-%m-%d %H:%M:%S.%f")[:-3] + " UTC"

        # Try to use a larger TrueType font; fall back to default if unavailable
        base_size = 30
        try:
            font = ImageFont.truetype("DejaVuSans.ttf", size=base_size)
        except Exception:
            base_font = ImageFont.load_default()
            font = (
                base_font.font_variant(size=base_size)
                if hasattr(base_font, "font_variant")
                else base_font
            )

        # Position bottom-left with padding and black background
        padding = 16
        text_bbox = draw.textbbox((0, 0), timestamp, font=font)
        text_w = text_bbox[2] - text_bbox[0]
        text_h = text_bbox[3] - text_bbox[1]
        x = padding
        y = stamped.height - text_h - padding
        draw.rectangle(
            [x - 10, y - 6, x + text_w + 10, y + text_h + 6],
            fill="black",
        )
        draw.text((x, y), timestamp, fill="white", font=font)

        buffer = io.BytesIO()
        stamped.save(buffer, format="JPEG", quality=85)
        msg = CompressedImage()
        msg.format = "jpeg"
        msg.data = buffer.getvalue()
        return msg

    def timer_callback(self):
        msg = self._overlay_timestamp(self.base_image)
        self.publisher_.publish(msg)
        self.get_logger().info(
            f"Published image with timestamp overlay ({len(msg.data)} bytes)"
        )


def main(args=None):
    rclpy.init(args=args)
    minimal_publisher = MinimalPublisher()
    rclpy.spin(minimal_publisher)
    minimal_publisher.destroy_node()
    rclpy.shutdown()


if __name__ == "__main__":
    main()
