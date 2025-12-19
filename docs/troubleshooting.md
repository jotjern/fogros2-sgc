# Troubleshooting

Common issues and solutions.

## Startup Issues

### "Group secret not found"

```
Error: Group secret not found: ./secrets/my-fleet/secret.key
Run: sgc init my-fleet
```

**Solution**: Generate the secret or copy it from another robot:

```bash
# Generate new secret
./scripts/generate_secret.sh my-fleet

# Or copy from another robot
scp user@other-robot:~/sgc/secrets/my-fleet ./secrets/
```

### "Cannot connect to signaling server"

**Check**:
1. Is the URL correct? (should be `ws://` or `wss://`)
2. Is the server running?
3. Is the port open in firewall?
4. Can you reach it? `curl -v ws://server:8000`

### "Cannot connect to routing server"

**Check**:
1. Is Redis running?
2. Is the address correct? (format: `host:port`)
3. Can you reach it? `redis-cli -h server -p 6379 ping`

### "Config validation failed"

Run `sgc check` for details. Common issues:
- Missing required fields
- Invalid role (must be `publisher`, `subscriber`, or `proxy`)
- Invalid signaling URL format

## Connection Issues

### No messages flowing

1. **Check both robots use the same secret**:
   ```bash
   md5sum secrets/my-fleet/secret.key  # Run on both robots, should match
   ```

2. **Check topic configuration**:
   - Names must match exactly (including leading `/`)
   - Types must match exactly
   - One must be `publisher`, other must be `subscriber`

3. **Check logs**:
   ```bash
   RUST_LOG=debug sgc router
   ```
   Look for "Establishing connection" and "Connection established" messages.

### "WebRTC setup failed"

Usually a network issue. Check:
- Both robots can reach the signaling server
- NAT traversal is working (check if behind symmetric NAT)
- Firewall allows UDP traffic

### Connection keeps dropping

- Check network stability
- Check Redis connection stability
- Look for "Connection removed" in logs to understand the pattern

## Performance Issues

### High latency

- Check network latency between robots
- Check if proxy nodes are needed (for many subscribers)
- Consider message size (large images may need compression)

### Messages missing

- WebRTC data channels are reliable but may have buffer limits
- Very high message rates may overflow buffers
- Consider reducing publish rate or message size

## Docker Issues

### Container keeps restarting

Check logs:
```bash
docker compose logs talker
docker compose logs listener
```

Common causes:
- Config file not found (check volume mounts)
- Secret not found (check volume mounts)
- Redis not ready yet (add `depends_on`)

### Cannot reach services

- Ensure containers are on the same Docker network
- Use service names (`rib`, `signal`) not `localhost`
- Check port mappings if accessing from host

## Debugging

### Enable debug logs

```bash
RUST_LOG=debug sgc router
```

### Check Redis state

```bash
# Connect to Redis
redis-cli -h localhost -p 8002

# List all keys
KEYS *

# Check routing state for a topic
GET <topic-id>-routing
```

### Monitor WebRTC signaling

Check signaling server logs:
```bash
docker compose logs signal
```

### Use the dashboard

```bash
docker compose up dashboard
# Open http://localhost:3000
```

Shows:
- Connected nodes
- Routing topology
- Message flow (if debug signals enabled)
