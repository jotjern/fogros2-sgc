# Usage: ./ratelimit.sh <rate>  (e.g., 1mbit, 100kbit) - limits upload bandwidth
docker exec -it fogros2-sgc-lite-talker-1 tc qdisc replace dev eth0 root tbf rate $1 burst 32kbit latency 400ms