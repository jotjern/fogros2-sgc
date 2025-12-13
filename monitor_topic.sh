#!/bin/bash
#
# Usage: ./monitor_topic.sh <service> <topic> <mode>
# Example: ./monitor_topic.sh talker /chatter bw
# Example: ./monitor_topic.sh talker /chatter hz
# Example: ./monitor_topic.sh talker /chatter echo

SERVICE="${1:-talker}"
TOPIC="${2:-/chatter}"
MODE="${3:-bw}"  # bw (bandwidth), hz (frequency), or echo (print messages)

echo "Monitoring $TOPIC on service $SERVICE (mode: $MODE)"
echo "Press Ctrl+C to stop"
echo ""

docker compose exec "$SERVICE" bash -c "source /opt/ros/humble/setup.bash && ros2 topic $MODE $TOPIC"
