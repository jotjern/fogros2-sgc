const express = require('express');
const http = require('http');
const { Server } = require('socket.io');
const Redis = require('ioredis');
const path = require('path');

const app = express();
const server = http.createServer(app);
const io = new Server(server, {
  cors: { origin: '*' }
});

app.use(express.static('public'));

app.get('/streams', (_req, res) => {
  res.sendFile(path.join(__dirname, 'public', 'streams.html'));
});

// Provide listener metadata for the streams page.
app.get('/streams-data', async (_req, res) => {
  const host = process.env.REDIS_HOST || 'localhost';
  const port = parseInt(process.env.REDIS_PORT || '6379', 10);
  const redis = new Redis({ host, port, lazyConnect: true });
  try {
    await redis.connect();
    const connectionKeys = await redis.keys('*-connections');
    const mappingKeys = await redis.keys('gdp-name-mapping:*');
    const publisherKeys = await redis.keys('*-publishers');

    const gdpMappings = {};
    for (const key of mappingKeys) {
      const gdp = key.replace('gdp-name-mapping:', '');
      const names = await redis.lrange(key, 0, -1);
      if (names.length > 0) gdpMappings[gdp] = names[0];
    }

    const publisherSet = new Set();
    for (const key of publisherKeys) {
      const pubs = await redis.lrange(key, 0, -1);
      pubs.forEach((p) => publisherSet.add(p));
    }

    const listeners = [];
    const seen = new Set();
    for (const key of connectionKeys) {
      const topicId = key.replace(/-connections$/, '');
      const connections = await redis.lrange(key, 0, -1);
      connections.forEach((conn) => {
        const parts = conn.split('-');
        if (parts.length !== 2) return;
        const subscriber = parts[1];
        if (publisherSet.has(subscriber)) return; // skip publishers
        const containerName = gdpMappings[subscriber] || subscriber;
        if (containerName.includes('proxy')) return; // skip proxies
        if (seen.has(subscriber)) return; // de-dup per listener
        seen.add(subscriber);
        const container = gdpMappings[subscriber] || subscriber;
        listeners.push({
          gdp: subscriber,
          container,
          topic: topicId,
          proxyUrl: `/stream/${container}/latest.jpg`,
        });
      });
    }

    res.json({ listeners });
  } catch (e) {
    res.status(500).json({ error: e.message });
  } finally {
    try { await redis.disconnect(); } catch (_) {}
  }
});

// Proxy latest frame from a listener container so the browser doesn't need Docker DNS access.
app.get('/stream/:container/latest.jpg', async (req, res) => {
  const container = req.params.container;
  const target = `http://${container}:8081/latest.jpg`;
  try {
    const response = await fetch(target);
    if (!response.ok) {
      res.status(502).send(`Upstream error: ${response.status}`);
      return;
    }
    const buf = Buffer.from(await response.arrayBuffer());
    res.setHeader('Content-Type', 'image/jpeg');
    res.setHeader('Cache-Control', 'no-store');
    res.send(buf);
  } catch (e) {
    res.status(502).send(`Failed to reach ${target}: ${e.message}`);
  }
});

io.on('connection', (socket) => {
  console.log('Client connected');
  let redis = null;
  let subscriber = null;
  let debugSub = null;
  let heartbeat = null;

  socket.on('connect-redis', async ({ host, port }) => {
    try {
      // Clean up existing connections
      if (redis) redis.disconnect();
      if (subscriber) subscriber.disconnect();
      if (debugSub) debugSub.disconnect();
      if (heartbeat) {
        clearInterval(heartbeat);
        heartbeat = null;
      }

      const redisConfig = {
        host: host || process.env.REDIS_HOST || 'localhost',
        port: port || parseInt(process.env.REDIS_PORT || '6379', 10),
        retryStrategy: () => null
      };

      redis = new Redis(redisConfig);
      subscriber = new Redis(redisConfig);

      redis.on('error', (err) => {
        socket.emit('redis-error', err.message);
      });

      redis.on('end', () => {
        socket.emit('redis-error', 'Redis connection closed');
      });

      subscriber.on('error', (err) => {
        console.error('Subscriber error:', err.message);
        socket.emit('redis-error', err.message);
      });

      await redis.ping();
      socket.emit('redis-connected');

      // Subscribe to keyspace notifications for any topic-scoped lists we use
      const dbIndex = 0;
      await subscriber.psubscribe(`__keyspace@${dbIndex}__:*connections`);
      await subscriber.psubscribe(`__keyspace@${dbIndex}__:*publishers`);
      await subscriber.psubscribe(`__keyspace@${dbIndex}__:*proxies`);
      await subscriber.psubscribe(`__keyspace@${dbIndex}__:*distress`);

      subscriber.on('pmessage', (pattern, channel, message) => {
        console.log(`Keyspace event: ${channel} -> ${message}`);
        fetchAndSend();
      });

      // Separate subscriber for pub/sub channels to avoid overlap with psubscribe
      debugSub = new Redis(redisConfig);
      debugSub.on('message', (channel, msg) => {
        if (channel !== 'debug-messages') return;
        // console.log('debug-messages ->', msg);
        try {
          const parsed = JSON.parse(msg);
          socket.emit('debug-event', parsed);
        } catch (e) {
          socket.emit('debug-event', { raw: msg });
        }
      });
      await debugSub.subscribe('debug-messages');
      console.log('Subscribed to debug-messages channel');

      heartbeat = setInterval(async () => {
        try {
          await redis.ping();
        } catch (e) {
          socket.emit('redis-error', 'Redis heartbeat failed');
        }
      }, 2000);

      // Initial fetch
      fetchAndSend();

    } catch (err) {
      socket.emit('redis-error', err.message);
    }
  });

  socket.on('refresh', () => {
    if (redis) fetchAndSend();
  });

  async function fetchAndSend() {
    try {
      // Discover all topic IDs via their connection lists: {topic_gdp}-connections
      const connectionKeys = await redis.keys('*-connections');

      const nodesSet = new Set();
      const proxySet = new Set();
      const connections = [];
      const topics = [];
      const distressedSet = new Set();

      for (const key of connectionKeys) {
        const topicId = key.replace(/-connections$/, '');
        const topicLabel = topicId;
        topics.push(topicLabel);

        const topicConnections = await redis.lrange(key, 0, -1);
        topicConnections.forEach((conn) => {
          const [publisher, subscriber] = conn.split('-');
          if (publisher) nodesSet.add(publisher);
          if (subscriber) nodesSet.add(subscriber);
          connections.push(`${publisher},${topicLabel},${subscriber}`);
        });

        const topicProxies = await redis.lrange(`${topicId}-proxies`, 0, -1);
        topicProxies.forEach((p) => {
          proxySet.add(p);
          nodesSet.add(p);
        });

        const topicPublishers = await redis.lrange(`${topicId}-publishers`, 0, -1);
        topicPublishers.forEach((p) => nodesSet.add(p));

        // Fetch distressed nodes for this topic
        const distressedNodes = await redis.lrange(`${topicId}-distress`, 0, -1);
        distressedNodes.forEach((node) => {
          distressedSet.add(node);
        });
      }

      // Fetch GDP name -> container name mappings
      const mappingKeys = await redis.keys('gdp-name-mapping:*');
      const gdpMappings = {};
      for (const key of mappingKeys) {
        const gdpName = key.replace('gdp-name-mapping:', '');
        const containerNames = await redis.lrange(key, 0, -1);
        if (containerNames.length > 0) {
          gdpMappings[gdpName] = containerNames[0]; // Use first entry
        }
      }

      // Find unconnected nodes (nodes that exist but have no connections)
      const connectedNodes = new Set();
      connections.forEach(conn => {
        const [publisher, , subscriber] = conn.split(',');
        if (publisher) connectedNodes.add(publisher);
        if (subscriber) connectedNodes.add(subscriber);
      });
      
      const allNodes = new Set([...nodesSet, ...proxySet]);
      const unconnectedNodes = Array.from(allNodes).filter(node => !connectedNodes.has(node));
      
      if (unconnectedNodes.length > 0) {
        console.log(`[Dashboard] Unconnected nodes (${unconnectedNodes.length}):`, unconnectedNodes.map(n => gdpMappings[n] || n).join(', '));
      }

      socket.emit('redis-data', {
        nodes: Array.from(nodesSet),
        proxies: Array.from(proxySet),
        connections,
        topics,
        gdpMappings,
        unconnectedNodes,
        distressedNodes: Array.from(distressedSet)
      });
    } catch (err) {
      socket.emit('redis-error', err.message);
    }
  }

  socket.on('disconnect', () => {
    console.log('Client disconnected');
    if (redis) redis.disconnect();
    if (subscriber) subscriber.disconnect();
    if (debugSub) debugSub.disconnect();
    if (heartbeat) {
      clearInterval(heartbeat);
      heartbeat = null;
    }
  });
});

const PORT = process.env.PORT || 3000;
server.listen(PORT, () => {
  console.log(`Server running at http://localhost:${PORT}`);
});
