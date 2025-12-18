# Security Model

How SGC protects your robot communications.

## Overview

SGC uses a **shared secret** model for group membership, combined with **WebRTC encryption** for data protection.

## What's Protected

| Aspect | Protection |
|--------|------------|
| **Payload data** | Encrypted (WebRTC DTLS) |
| **Topic names** | Hidden from servers (hashed) |
| **Message types** | Hidden from servers (hashed) |
| **Group membership** | Shared secret required |

## How It Works

### Group Secret

All robots in a fleet share a secret file (`secret.key`). This acts as a group membership key:

1. Topic identity is computed as: `hash(topic_name + topic_type + secret)`
2. Only robots with the same secret compute the same identity
3. Servers see only the hashed identity, not the actual topic name

### What Servers See

The signaling and routing servers see:
- Random-looking 4-byte identifiers (hashed topic IDs)
- Connection patterns (which IDs connect to which)
- Timing information

They **cannot** see:
- Actual topic names (e.g., `/camera/image`)
- Message types
- Message content

### Data Encryption

Actual message data is encrypted using WebRTC's built-in DTLS:
- Peer-to-peer encryption
- Perfect forward secrecy
- Standard WebRTC security

## Trust Model

**All robots with the shared secret are equally trusted.** There is no individual robot identity - any robot with the secret can act as any role (publisher, subscriber, proxy).

| Component | Trust Level | Can be Shared? |
|-----------|-------------|----------------|
| **Robots in group** | Fully trusted | N/A |
| **Signaling server** | Semi-trusted (sees connection patterns) | ✅ Yes - safe to share |
| **Routing server (Redis)** | Must be protected | ❌ No - run privately |
| **Network** | Untrusted | N/A |

### Why Redis Must Be Private

The routing server (Redis) stores routing state for all connected fleets. Without protection, anyone with Redis access can:
- Read/modify routing entries
- Disrupt other fleets
- Inject fake publishers/subscribers

**Always run Redis on private infrastructure or enable authentication.**

### Why Signaling Can Be Shared

The signaling server only relays WebRTC handshake messages:
- It cannot decrypt message payloads (DTLS encrypted)
- Connection IDs are derived from secrets (unpredictable to outsiders)
- Worst case: an attacker sees connection timing patterns

## Recommendations

### Protect Your Secret

The group secret is like a password. Anyone with it can:
- Join your robot group
- Receive messages from your robots
- Send messages to your robots

Store it securely. Don't commit it to git. Rotate it if compromised.

**Important**: There is no way to revoke a single robot. If one robot is compromised, you must rotate the secret on ALL robots.

### Deployment Architecture

For production deployments:

| Component | Recommendation |
|-----------|----------------|
| **Signaling server** | Can use shared/public instance |
| **Redis (routing)** | Run privately per fleet, or use AUTH |

### Protecting Redis

Option 1: Run Redis on private network (recommended):
```bash
# Only accessible from your robots via VPN/private network
docker run -d --name redis -p 127.0.0.1:6379:6379 redis:6
```

Option 2: Enable Redis AUTH:
```bash
# Start Redis with password
docker run -d --name redis redis:6 --requirepass YOUR_PASSWORD
```

Then in your config:
```toml
routing_server = "redis://:YOUR_PASSWORD@your-host:6379"
```

## Limitations

### Not a PKI

SGC does not use certificate verification. The "secret" is just a shared file used for hashing. This is simpler but provides different guarantees than a full PKI:

- No individual robot identity verification
- No certificate revocation
- Compromise of secret affects entire group

### No Key Rotation Mechanism

There is currently no automated way to rotate secrets. To rotate:
1. Generate new secret
2. Deploy to all robots
3. Restart all robots simultaneously

### Identifier Size

Topic identifiers are 4 bytes (32 bits). This is sufficient for typical deployments but could theoretically have collisions with many thousands of topics.

### Signaling Server MITM

If an attacker controls the signaling server, they could potentially man-in-the-middle WebRTC connections (DTLS certificates are not pinned). Mitigations:
- Run your own signaling server for sensitive deployments
- Use network-level security (TLS, VPN)

## Suitable Use Cases

SGC is appropriate for:
- Research and academic use
- Controlled fleet environments
- Deployments where physical security of robots is maintained

SGC may not be suitable for:
- Untrusted robot operators
- High-security applications requiring individual robot identity
- Fleets where individual robots might be compromised and need revocation
