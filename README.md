# FogROS2-SGC

Secure Global Connectivity for ROS2. Connect robots across different networks, locations, and cloud providers.

[[Paper](https://arxiv.org/abs/2306.17157)] [[Video](https://youtu.be/hVVFVGLcK0c)] [[Website](https://sites.google.com/view/fogros2-sgc)]

## What It Does

SGC bridges ROS2 networks that can't communicate directly:
- Robots behind different NATs/firewalls
- Robots on different cloud providers
- Edge devices and cloud servers

Data flows through encrypted WebRTC connections. No VPN or port forwarding required.

## Quick Demo

```bash
# Generate a group secret
./scripts/generate_secret.sh demo

# Run the demo (talker + listener in separate containers)
docker compose up --build
```

See messages flowing between isolated ROS2 networks:
```bash
docker compose logs listener | grep "I heard"
```

## How It Works

```
┌─────────────────┐                      ┌─────────────────┐
│   Robot A       │                      │   Robot B       │
│                 │                      │                 │
│  ROS2 Publisher │                      │  ROS2 Subscriber│
│       │         │                      │       ▲         │
│       ▼         │   WebRTC (encrypted) │       │         │
│   SGC Router ───┼──────────────────────┼─► SGC Router    │
│                 │                      │                 │
└─────────────────┘                      └─────────────────┘
```

1. Robot A's SGC subscribes to local `/topic`
2. Messages are forwarded over encrypted WebRTC
3. Robot B's SGC publishes to local `/topic`

## Installation

### Prerequisites

- Rust 1.70+
- ROS2 Humble (or later)

### Build

```bash
# Install Rust
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh

# Build
cargo build --release
```

## Usage

### 1. Generate a group secret

```bash
./scripts/generate_secret.sh my-fleet
```

Copy `./secrets/my-fleet/` to all robots.

### 2. Create config files

On Robot A (publisher):
```toml
group_secret = "my-fleet"
signaling_server = "ws://your-server:8000"
routing_server = "your-server:6379"

[[topics]]
name = "/camera/image"
type = "sensor_msgs/msg/Image"
role = "publisher"
```

On Robot B (subscriber):
```toml
group_secret = "my-fleet"
signaling_server = "ws://your-server:8000"
routing_server = "your-server:6379"

[[topics]]
name = "/camera/image"
type = "sensor_msgs/msg/Image"
role = "subscriber"
```

### 3. Run

```bash
# Validate configuration
SGC_CONFIG=robot.toml ./target/release/sgc check

# Start the router
SGC_CONFIG=robot.toml ./target/release/sgc router
```

## Documentation

- [Quickstart](docs/quickstart.md) - Docker demo
- [Deployment Guide](docs/deployment.md) - Full setup
- [Configuration](docs/configuration.md) - Config reference
- [Security](docs/security.md) - Security model
- [Troubleshooting](docs/troubleshooting.md) - Common issues

## Commands

```bash
sgc init <name>     # Generate new group secret
sgc check           # Validate config, test connectivity
sgc router          # Start the router
sgc config          # Show current configuration
```

## License

Apache 2.0
