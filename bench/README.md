# FogROS2-SGC Benchmarks

Benchmarks for evaluating hierarchical routing performance.

## Quick Start

```bash
cd bench
pip install -r requirements.txt
python run_benchmarks.py --clean
```

## Benchmarks

| # | Script | Paper Section | Output |
|---|--------|--------------|--------|
| 1 | `benchmark1_scalability.py` | Required Bandwidth | `benchmark1_scalability.png` |
| 2 | `benchmark2_latency.py` | Transmission Latency | `benchmark2_latency.png` |
| 3 | `benchmark3_join.py` | Join Latency | `benchmark3_join.png` |
| 4 | `benchmark4_fanout.py` | (Optional) Fanout tuning | `benchmark4_fanout.png` |
| 5 | `benchmark5_recovery.py` | (Optional) Failure recovery | `benchmark5_recovery.png` |

## Usage

```bash
# Run all benchmarks
python run_benchmarks.py

# Run with clean slate (removes cached results)
python run_benchmarks.py --clean

# Run specific benchmarks (1, 2, 3 for paper)
python run_benchmarks.py 1 2 3

# Run just one
python run_benchmarks.py 1
```

## What Each Benchmark Measures

### 1. Scalability (Required Bandwidth)
**Question**: Does hierarchical routing reduce publisher bandwidth?

- Measures publisher network TX as subscriber count increases
- Compares hierarchical (fanout=3) vs direct routing
- Expected: Hierarchical flattens ~8 subscribers, direct grows linearly

### 2. Latency (Transmission Latency)
**Question**: What is the latency cost of hierarchical routing?

- Measures end-to-end message latency (p50, p95)
- Shows tradeoff: hierarchical adds latency due to tree hops
- Expected: Hierarchical ~100ms higher than direct at 50 subscribers

### 3. Join Latency
**Question**: How long until a new subscriber receives its first message?

- Measures time from subscription to first message
- At 10Hz publish rate, theoretical minimum = 50ms
- Compares connection establishment overhead
- Expected: Hierarchical has higher p95 due to tree construction

### 4. Fanout Tuning
**Question**: What fanout value gives the best latency-cost tradeoff?

- Tests fanout values 2, 3, 4, 5, 8, 10
- Higher fanout = lower latency, fewer proxies
- Helps operators tune for their deployment

### 5. Recovery
**Question**: How quickly does the system recover from subscriber failures?

- Stops 50% of subscribers, then restarts them
- Measures time to return to 90% baseline throughput
- Shows system resilience

## Output

Results are saved to `results/`:
- `benchmarkN_*.json` — Raw data
- `benchmarkN_*.png` — Publication-ready figures (300 DPI)
- `benchmark_run.log` — Execution log

## Configuration

Key parameters are defined at the top of each benchmark script:
- `SUBSCRIBER_COUNTS` — List of N values to test
- `FANOUT` — Hierarchical fanout factor (default: 3)
- `MEASURE_SECS` — Duration of each measurement
- `TIMEOUT_SECS` — Max wait for subscribers to connect
