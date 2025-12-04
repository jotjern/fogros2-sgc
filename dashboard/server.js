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

  socket.on('connect-redis', async ({ host, port }) => {
    try {
      // Clean up existing connections
      if (redis) redis.disconnect();
      if (subscriber) subscriber.disconnect();

      const redisConfig = {
        host: host || 'localhost',
        port: port || 6379,
        retryStrategy: () => null
      };

      redis = new Redis(redisConfig);
      subscriber = new Redis(redisConfig);

      redis.on('error', (err) => {
        socket.emit('redis-error', err.message);
      });

      subscriber.on('error', (err) => {
        console.error('Subscriber error:', err.message);
      });

      await redis.ping();
      socket.emit('redis-connected');

      // Subscribe to keyspace notifications for the keys we care about
      const dbIndex = 0;
      await subscriber.psubscribe(`__keyspace@${dbIndex}__:nodes`);
      await subscriber.psubscribe(`__keyspace@${dbIndex}__:proxies`);
      await subscriber.psubscribe(`__keyspace@${dbIndex}__:connections`);
      await subscriber.psubscribe(`__keyspace@${dbIndex}__:topics`);

      subscriber.on('pmessage', (pattern, channel, message) => {
        console.log(`Keyspace event: ${channel} -> ${message}`);
        fetchAndSend();
      });

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
      const [nodes, proxies, connections, topics] = await Promise.all([
        redis.lrange('nodes', 0, -1),
        redis.lrange('proxies', 0, -1),
        redis.lrange('connections', 0, -1),
        redis.lrange('topics', 0, -1)
      ]);

      socket.emit('redis-data', { nodes, proxies, connections, topics });
    } catch (err) {
      socket.emit('redis-error', err.message);
    }
  }

  socket.on('disconnect', () => {
    console.log('Client disconnected');
    if (redis) redis.disconnect();
    if (subscriber) subscriber.disconnect();
  });
});

const PORT = process.env.PORT || 3000;
server.listen(PORT, () => {
  console.log(`Server running at http://localhost:${PORT}`);
});