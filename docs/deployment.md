# Deployment Guide

Set up SGC for your own robot fleet.

## Overview

You need:
1. **Signaling server**: Relays WebRTC handshakes (can be shared/public)
2. **Redis server**: Stores routing state (must be private per fleet)
3. **Group secret**: Shared by all robots in your fleet
4. **Config file**: One per robot, specifying its topics

## Step 1: Deploy Infrastructure

### Signaling Server

The signaling server can be shared across fleets - it only relays encrypted handshakes.

**Option A: Use a public signaling server**
```toml
signaling_server = "ws://signal.example.com:8000"
```

**Option B: Self-hosted**
```bash
docker compose up -d signal
# Exposes port 8005
```

### Redis (Routing Server)

**Redis must be private to your fleet.** See [Security](security.md) for details.

**Option A: Local Redis (single-site fleet)**
```bash
docker run -d --name redis -p 6379:6379 redis:6
```

**Option B: Redis with AUTH (multi-site fleet)**
```bash
docker run -d --name redis -p 6379:6379 redis:6 --requirepass YOUR_SECRET_PASSWORD
```

Config:
```toml
routing_server = "redis://:YOUR_SECRET_PASSWORD@your-redis-host:6379"
```

**Option C: Private network**

Run Redis on a private network accessible only to your robots (VPN, private subnet, etc.).

## Step 2: Generate Group Secret

On any machine:

```bash
./scripts/generate_secret.sh my-fleet
```

This creates `./secrets/my-fleet/secret.key`.

Copy this directory to all robots:
```bash
scp -r ./secrets/my-fleet user@robot1:~/sgc/secrets/
scp -r ./secrets/my-fleet user@robot2:~/sgc/secrets/
```

## Step 3: Configure Each Robot

Create a config file for each robot. Example for a camera robot:

```toml
# robot1.toml - Camera robot
group_secret = "my-fleet"
signaling_server = "ws://YOUR_SERVER:8005"
routing_server = "YOUR_SERVER:8003"

[[topics]]
name = "/camera/image"
type = "sensor_msgs/msg/Image"
role = "publisher"

[[topics]]
name = "/cmd_vel"
type = "geometry_msgs/msg/Twist"
role = "subscriber"
```

And for a control station:

```toml
# station.toml - Control station
group_secret = "my-fleet"
signaling_server = "ws://YOUR_SERVER:8005"
routing_server = "YOUR_SERVER:8003"

[[topics]]
name = "/camera/image"
type = "sensor_msgs/msg/Image"
role = "subscriber"

[[topics]]
name = "/cmd_vel"
type = "geometry_msgs/msg/Twist"
role = "publisher"
```

## Step 4: Build SGC

On each robot:

```bash
# Install Rust
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source ~/.cargo/env

# Install ROS2 (if not already)
# See: https://docs.ros.org/en/humble/Installation.html

# Build SGC
cd fogros2-sgc
cargo build --release
```

## Step 5: Run

On each robot:

```bash
# Validate config first
SGC_CONFIG=robot1.toml ./target/release/sgc check

# If all checks pass, run the router
SGC_CONFIG=robot1.toml ./target/release/sgc router
```

## Verifying Connections

Use the dashboard to visualize connections:

```bash
# On a machine that can reach Redis
docker compose up dashboard
# Open http://localhost:3001
```

## Troubleshooting

### "Group secret not found"

Run `sgc init <name>` or copy the secret from another robot.

### "Cannot connect to signaling server"

- Check the URL is correct
- Check firewall allows the port
- Check the signaling server is running

### "Cannot connect to routing server"

- Check Redis is running
- Check the address/port in config
- Check firewall rules

### No messages flowing

- Check both robots use the same `group_secret`
- Check topic names and types match exactly
- Check one is `publisher` and one is `subscriber`
