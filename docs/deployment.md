# Deployment Guide

Set up SGC for your own robot fleet.

## Overview

You need:
1. **Infrastructure**: Signaling server + Redis (one set, shared by all robots)
2. **Group secret**: Shared by all robots in your fleet
3. **Config file**: One per robot, specifying its topics

## Step 1: Deploy Infrastructure

### Option A: Use Public Servers (Testing Only)

For quick testing, use Berkeley's public servers:
- Signaling: `ws://3.18.194.127:8000`
- Routing: `3.18.194.127:8002`

**Warning**: These are for testing only. Don't use for production.

### Option B: Self-Hosted (Recommended)

Deploy your own servers using Docker:

```bash
# On a server with public IP (or accessible from all robots)
docker compose up -d rib signal
```

This exposes:
- Signaling server on port 8005
- Redis on port 8003

Use these URLs in your config:
```toml
signaling_server = "ws://YOUR_SERVER_IP:8005"
routing_server = "YOUR_SERVER_IP:8003"
```

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
