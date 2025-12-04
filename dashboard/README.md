# Redis Graph Visualizer - Sample Configurations

Sample Redis configurations for testing the graph visualizer.

---

## Simple Fanout

A single publisher distributing to multiple subscribers through one proxy.
```
      N1 (pub)
       │
      P1
     / | \
   N2 N3 N4
```
```bash
redis-cli -p 8002 FLUSHDB

redis-cli -p 8002 RPUSH nodes N1 N2 N3 N4
redis-cli -p 8002 RPUSH proxies P1
redis-cli -p 8002 RPUSH topics "N1,sensor"

redis-cli -p 8002 RPUSH connections "N1,sensor,P1"
redis-cli -p 8002 RPUSH connections "P1,sensor,N2"
redis-cli -p 8002 RPUSH connections "P1,sensor,N3"
redis-cli -p 8002 RPUSH connections "P1,sensor,N4"
```

---

## Tree with Fanout of 3

A two-level proxy tree for distributing high-bandwidth streams.
```
                N1 (pub)
                 │
                P1
           /    |    \
         P2    P3     P4
        /|\   /|\    /|\
      N2-N4 N5-N7  N8-N10
```
```bash
redis-cli -p 8002 FLUSHDB

redis-cli -p 8002 RPUSH nodes N1 N2 N3 N4 N5 N6 N7 N8 N9 N10
redis-cli -p 8002 RPUSH proxies P1 P2 P3 P4
redis-cli -p 8002 RPUSH topics "N1,video"

redis-cli -p 8002 RPUSH connections "N1,video,P1"
redis-cli -p 8002 RPUSH connections "P1,video,P2"
redis-cli -p 8002 RPUSH connections "P1,video,P3"
redis-cli -p 8002 RPUSH connections "P1,video,P4"
redis-cli -p 8002 RPUSH connections "P2,video,N2"
redis-cli -p 8002 RPUSH connections "P2,video,N3"
redis-cli -p 8002 RPUSH connections "P2,video,N4"
redis-cli -p 8002 RPUSH connections "P3,video,N5"
redis-cli -p 8002 RPUSH connections "P3,video,N6"
redis-cli -p 8002 RPUSH connections "P3,video,N7"
redis-cli -p 8002 RPUSH connections "P4,video,N8"
redis-cli -p 8002 RPUSH connections "P4,video,N9"
redis-cli -p 8002 RPUSH connections "P4,video,N10"
```

---

## Bidirectional Communication

Two nodes communicating bidirectionally through a shared proxy.
```
N1 ──sensor──> P1 ──sensor──> N2
N1 <──command── P1 <──command── N2
```
```bash
redis-cli -p 8002 FLUSHDB

redis-cli -p 8002 RPUSH nodes N1 N2
redis-cli -p 8002 RPUSH proxies P1
redis-cli -p 8002 RPUSH topics "N1,sensor" "N2,command"

redis-cli -p 8002 RPUSH connections "N1,sensor,P1"
redis-cli -p 8002 RPUSH connections "P1,sensor,N2"
redis-cli -p 8002 RPUSH connections "N2,command,P1"
redis-cli -p 8002 RPUSH connections "P1,command,N1"
```

---

## Multi-Topic Network

Multiple topics sharing proxy infrastructure.
```bash
redis-cli -p 8002 FLUSHDB

redis-cli -p 8002 RPUSH nodes N1 N2 N3 N4
redis-cli -p 8002 RPUSH proxies P1 P2
redis-cli -p 8002 RPUSH topics "N1,camera" "N2,lidar"

# camera: N1 -> P1 -> P2 -> N3, N4
redis-cli -p 8002 RPUSH connections "N1,camera,P1"
redis-cli -p 8002 RPUSH connections "P1,camera,P2"
redis-cli -p 8002 RPUSH connections "P2,camera,N3"
redis-cli -p 8002 RPUSH connections "P2,camera,N4"

# lidar: N2 -> P1 -> N3, and N2 -> N4 direct
redis-cli -p 8002 RPUSH connections "N2,lidar,P1"
redis-cli -p 8002 RPUSH connections "P1,lidar,N3"
redis-cli -p 8002 RPUSH connections "N2,lidar,N4"
```