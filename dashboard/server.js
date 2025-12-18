const express = require('express');
const http = require('http');
const { Server } = require('socket.io');
const Redis = require('ioredis');

const app = express();
const server = http.createServer(app);
const io = new Server(server, {
  cors: { origin: '*' }
});

// Avoid stale dashboard assets when schema changes (prevents old JS calling `.split()`).
app.use(
  express.static('public', {
    etag: false,
    maxAge: 0,
    setHeaders: (res) => {
      res.setHeader('Cache-Control', 'no-store, max-age=0');
    }
  })
);

io.on('connection', (socket) => {
  console.log('Client connected');
  let redis = null;
  let subscriber = null;
  let debugSub = null;
  let heartbeat = null;
  let lastError = null;

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
        console.error('[Dashboard] Redis client error:', err);
        lastError = err;
        socket.emit('redis-error', err.message);
      });

      redis.on('end', () => {
        const msg = 'Redis connection closed';
        console.error('[Dashboard] Redis client ended:', msg);
        lastError = new Error(msg);
        socket.emit('redis-error', 'Redis connection closed');
      });

      subscriber.on('error', (err) => {
        console.error('[Dashboard] Redis subscriber error:', err);
        lastError = err;
        socket.emit('redis-error', err.message);
      });

      await redis.ping();
      socket.emit('redis-connected');

      // Subscribe to keyspace notifications for topic routing + mappings
      const dbIndex = 0;
      await subscriber.psubscribe(`__keyspace@${dbIndex}__:*routing`);
      await subscriber.psubscribe(`__keyspace@${dbIndex}__:gdpname_map`);
      // Optional legacy/debug key
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
          console.error('[Dashboard] Redis heartbeat failed:', e);
          lastError = e;
          socket.emit('redis-error', 'Redis heartbeat failed');
        }
      }, 2000);

      // Initial fetch
      fetchAndSend();

    } catch (err) {
      console.error('[Dashboard] connect-redis failed:', err);
      lastError = err;
      socket.emit('redis-error', err.message);
    }
  });

  socket.on('refresh', () => {
    if (redis) fetchAndSend();
  });

  async function fetchAndSend() {
    try {
      // Discover all topics via their routing state: {topic_gdp}-routing
      const routingKeys = await redis.keys('*-routing');

      const nodesSet = new Set();
      const proxySet = new Set();
      const publisherSet = new Set();
      const subscriberSet = new Set();
      const connections = [];
      const topics = [];
      const distressedSet = new Set();

      for (const key of routingKeys) {
        const topicId = key.replace(/-routing$/, '');
        const topicLabel = topicId;
        topics.push(topicLabel);

        const raw = await redis.get(key);
        let state = null;
        try {
          state = raw ? JSON.parse(raw) : null;
        } catch (e) {
          state = null;
        }
        const edges = (state && Array.isArray(state.edges)) ? state.edges : [];
        const proxies = (state && Array.isArray(state.proxies)) ? state.proxies : [];
        const publishers = (state && Array.isArray(state.publishers)) ? state.publishers : [];

        edges.forEach((conn) => {
          // Current routing schema: edges are objects: { parent, child }
          if (!conn || typeof conn !== 'object') {
            return;
          }
          const publisher = conn.parent || null;
          const subscriber = conn.child || null;
          if (!publisher || !subscriber) return;

          nodesSet.add(publisher);
          nodesSet.add(subscriber);
          publisherSet.add(publisher);
          subscriberSet.add(subscriber);
          connections.push({ source: publisher, topic: topicLabel, target: subscriber });
        });

        proxies.forEach((p) => {
          proxySet.add(p);
          nodesSet.add(p);
        });

        publishers.forEach((p) => {
          nodesSet.add(p);
          publisherSet.add(p);
        });

        // Fetch distressed nodes for this topic
        try {
          const distressedNodes = await redis.lrange(`${topicId}-distress`, 0, -1);
          distressedNodes.forEach((node) => distressedSet.add(node));
        } catch (e) {
          // ignore if key/type doesn't exist
        }
      }

      // Fetch GDP name -> container name mappings (stored as a single Redis hash)
      const gdpMappings = await redis.hgetall('gdpname_map');

      // Fetch nodes that have received data (stored as a Redis set by listeners)
      let nodesReceivedData = [];
      try {
        nodesReceivedData = await redis.smembers('nodes_received_data');
      } catch (e) {
        // ignore if key doesn't exist
      }

      // Find unconnected nodes (nodes that exist but have no connections)
      const connectedNodes = new Set();
      connections.forEach(conn => {
        if (!conn || typeof conn !== 'object') return;
        if (conn.source) connectedNodes.add(conn.source);
        if (conn.target) connectedNodes.add(conn.target);
      });
      
      const allNodes = new Set([...nodesSet, ...proxySet]);
      const unconnectedNodes = Array.from(allNodes).filter(node => !connectedNodes.has(node));
      
      if (unconnectedNodes.length > 0) {
        console.log(`[Dashboard] Unconnected nodes (${unconnectedNodes.length}):`, unconnectedNodes.map(n => gdpMappings[n] || n).join(', '));
      }

      socket.emit('redis-data', {
        nodes: Array.from(nodesSet),
        proxies: Array.from(proxySet),
        publishers: Array.from(publisherSet),
        subscribers: Array.from(subscriberSet),
        connections,
        topics,
        gdpMappings,
        unconnectedNodes,
        distressedNodes: Array.from(distressedSet),
        nodesReceivedData
      });
    } catch (err) {
      console.error('[Dashboard] fetchAndSend failed:', err);
      lastError = err;
      socket.emit('redis-error', err.message);
    }
  }

  // Allow UI to request last server-side error details (for debugging).
  socket.on('get-last-error', () => {
    if (!lastError) return;
    socket.emit('server-error', {
      message: lastError.message || String(lastError),
      stack: lastError.stack || null
    });
  });

  // Client-side error reporting (so we don't lose UI exceptions).
  socket.on('client-error', (payload) => {
    try {
      console.error('[Dashboard] client-error:', payload);
      lastError = new Error(payload?.message || 'client-error');
      if (payload?.stack) lastError.stack = payload.stack;
    } catch (e) {
      console.error('[Dashboard] client-error handler failed:', e);
    }
  });

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
