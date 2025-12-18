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

| Component | Trust Level |
|-----------|-------------|
| **Other robots in group** | Fully trusted (shared secret) |
| **Signaling server** | Semi-trusted (sees connection patterns) |
| **Routing server** | Semi-trusted (sees connection patterns) |
| **Network** | Untrusted (all traffic encrypted) |

## Recommendations

### Protect Your Secret

The group secret is like a password. Anyone with it can:
- Join your robot group
- Receive messages from your robots
- Send messages to your robots

Store it securely. Don't commit it to git. Rotate it if compromised.

### Use Your Own Servers

Public servers are for testing only. For production:
- Deploy your own signaling + Redis servers
- Run them on infrastructure you control
- Consider adding TLS to signaling server

### Network Segmentation

For additional security:
- Run signaling/Redis on a private network
- Use VPN or firewall rules to restrict access
- Monitor connection logs for anomalies

## Limitations

### Not a PKI

SGC does not use certificate verification. The "secret" is just a shared file used for hashing. This is simpler but provides different guarantees than a full PKI:

- No individual robot identity verification
- No certificate revocation
- Compromise of secret affects entire group

### Identifier Size

Topic identifiers are 4 bytes (32 bits). This is sufficient for typical deployments but could theoretically have collisions with many thousands of topics.

### Signaling Server Trust

If an attacker controls your signaling server, they could potentially:
- See which robots are connecting
- Delay or drop signaling messages
- Attempt connection manipulation

Use your own signaling server for sensitive deployments.
