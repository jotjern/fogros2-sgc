# Configuration Files

Example configurations for different scenarios.

## Configuration Format

```toml
# Shared secret for your robot fleet (all robots must use the same)
group_secret = "my-fleet"

# Infrastructure servers
signaling_server = "ws://signal.example.com:8000"
routing_server = "rib.example.com:6379"

# Topics to bridge
[[topics]]
name = "/camera/image"
type = "sensor_msgs/msg/Image"
role = "publisher"    # This robot publishes this topic

[[topics]]
name = "/cmd_vel"
type = "geometry_msgs/msg/Twist"
role = "subscriber"   # This robot subscribes to this topic
```

## Roles

- **publisher**: This robot publishes the topic. Remote subscribers will receive it.
- **subscriber**: This robot subscribes to the topic from remote publishers.
- **proxy**: Relay node for scaling (advanced use).

## Files

- `talker.toml` / `listener.toml`: For use with public Berkeley servers (testing)
- `talker-docker.toml` / `listener-docker.toml` / `proxy-docker.toml`: Docker demo
- `automatic.toml`: Default config (no topics configured)
