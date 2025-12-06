const express = require('express');
const http = require('http');
const { Server } = require('socket.io');
const Redis = require('ioredis');

const app = express();
const server = http.createServer(app);
const io = new Server(server, {
  cors: { origin: '*' }
});

app.use(express.static('public'));

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

      subscriber.on('pmessage', (pattern, channel, message) => {
        console.log(`Keyspace event: ${channel} -> ${message}`);
        fetchAndSend();
      });

      // Separate subscriber for pub/sub channels to avoid overlap with psubscribe
      debugSub = new Redis(redisConfig);
      debugSub.on('message', (channel, msg) => {
        if (channel !== 'debug-messages') return;
        console.log('debug-messages ->', msg);
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
      }

      socket.emit('redis-data', {
        nodes: Array.from(nodesSet),
        proxies: Array.from(proxySet),
        connections,
        topics
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
