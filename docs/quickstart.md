# Quickstart

Run the demo in under 5 minutes using Docker.

## Prerequisites

- Docker and Docker Compose installed
- That's it.

## Run the Demo

```bash
# Clone the repo
git clone https://github.com/BerkeleyAutomation/fogros2-sgc.git
cd fogros2-sgc

# Generate a group secret for the demo
./scripts/generate_secret.sh demo

# Build and run
docker compose up --build
```

This starts:
- A ROS2 talker (publisher)
- A ROS2 listener (subscriber)
- SGC routers connecting them
- Redis (routing state)
- Signaling server (WebRTC coordination)

## Verify It Works

In another terminal:

```bash
# Check listener logs - should show received messages
docker compose logs listener | grep "I heard"
```

You should see output like:
```
listener  | [INFO] I heard: [Hello World: 42]
listener  | [INFO] I heard: [Hello World: 43]
```

## What Just Happened?

The talker and listener run in separate Docker containers with isolated ROS2 networks. They cannot communicate directly. SGC bridges them:

1. Talker publishes to `/chatter` locally
2. SGC router subscribes to `/chatter`, forwards via WebRTC
3. SGC router on listener side receives, publishes to local `/chatter`
4. Listener receives messages

## Next Steps

- [Deployment Guide](deployment.md) - Set up your own robots
- [Configuration Reference](configuration.md) - All config options
- [Security Model](security.md) - How security works
