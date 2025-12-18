# Configuration Reference

## Config File Location

SGC looks for config in this order:
1. `--config` CLI argument
2. `SGC_CONFIG` environment variable (relative to `./src/resources/`)
3. `./src/resources/automatic.toml`

## Full Example

```toml
# Required: Name of the group secret directory
# Loads ./secrets/<group_secret>/secret.key
group_secret = "my-fleet"

# Required: WebSocket URL for signaling server
signaling_server = "ws://signal.example.com:8000"

# Required: Redis address for routing state
routing_server = "rib.example.com:6379"

# Topics to bridge (at least one required)
[[topics]]
name = "/camera/image"           # ROS topic name
type = "sensor_msgs/msg/Image"   # ROS message type
role = "publisher"               # publisher, subscriber, or proxy
```

## Fields

### `group_secret`

Name of the directory under `./secrets/` containing `secret.key`.

All robots that should communicate must use the same secret. Generate with:
```bash
./scripts/generate_secret.sh my-fleet
```

### `signaling_server`

WebSocket URL for the signaling server. Used to coordinate WebRTC connections.

Format: `ws://host:port` or `wss://host:port`

### `routing_server`

Redis server address for routing state.

Format: `host:port`

### `[[topics]]`

List of topics to bridge. Each topic has:

| Field | Description |
|-------|-------------|
| `name` | ROS topic name (e.g., `/camera/image`) |
| `type` | ROS message type (e.g., `sensor_msgs/msg/Image`) |
| `role` | `publisher`, `subscriber`, or `proxy` |

**Roles:**

- `publisher`: This robot publishes the topic. SGC subscribes locally and forwards to remote subscribers.
- `subscriber`: This robot wants to receive the topic. SGC receives from remote and publishes locally.
- `proxy`: Relay node for scaling. Receives from upstream, forwards to downstream.

## Environment Variables

| Variable | Description |
|----------|-------------|
| `SGC_CONFIG` | Config file name (relative to `./src/resources/`) |
| `RUST_LOG` | Log level: `error`, `warn`, `info`, `debug`, `trace` |

## Examples

### Two robots sharing a camera

Robot 1 (has camera):
```toml
group_secret = "fleet"
signaling_server = "ws://server:8000"
routing_server = "server:6379"

[[topics]]
name = "/camera/image"
type = "sensor_msgs/msg/Image"
role = "publisher"
```

Robot 2 (views camera):
```toml
group_secret = "fleet"
signaling_server = "ws://server:8000"
routing_server = "server:6379"

[[topics]]
name = "/camera/image"
type = "sensor_msgs/msg/Image"
role = "subscriber"
```

### Bidirectional communication

Robot (camera + receives commands):
```toml
group_secret = "fleet"
signaling_server = "ws://server:8000"
routing_server = "server:6379"

[[topics]]
name = "/camera/image"
type = "sensor_msgs/msg/Image"
role = "publisher"

[[topics]]
name = "/cmd_vel"
type = "geometry_msgs/msg/Twist"
role = "subscriber"
```

Control station:
```toml
group_secret = "fleet"
signaling_server = "ws://server:8000"
routing_server = "server:6379"

[[topics]]
name = "/camera/image"
type = "sensor_msgs/msg/Image"
role = "subscriber"

[[topics]]
name = "/cmd_vel"
type = "geometry_msgs/msg/Twist"
role = "publisher"
```
