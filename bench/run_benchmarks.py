#!/usr/bin/env python3
"""
FogROS2-SGC Benchmark Suite
===========================

Runs all benchmarks and generates plots for the paper.

Usage:
    python run_benchmarks.py           # Run all benchmarks
    python run_benchmarks.py --clean   # Clear cache and re-run all
    python run_benchmarks.py 1 2       # Run only benchmarks 1 and 2
"""
import os
import subprocess
import sys
from datetime import datetime


ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
RESULTS_DIR = os.path.join(ROOT, "bench", "results")

BENCHMARKS = [
    # Core benchmarks for paper
    ("benchmark1_scalability.py", "Bandwidth Scalability: Publisher TX vs subscriber count"),
    ("benchmark2_latency.py", "Transmission Latency: End-to-end latency"),
    ("benchmark3_join.py", "Join Latency: Time to first message"),
    ("benchmark4_fanout.py", "Fanout Tuning: Latency vs fanout factor"),
    ("benchmark5_storm.py", "Storm Resilience: Recovery + tree balance"),
    ("benchmark6_payload.py", "Payload Size: Bandwidth vs message size"),
    ("benchmark7_overhead.py", "Protocol Overhead: Goodput vs raw throughput"),
]


def clear_cache():
    """Remove cached JSON files."""
    if not os.path.exists(RESULTS_DIR):
        return
    removed = 0
    for f in os.listdir(RESULTS_DIR):
        if f.endswith(".json"):
            try:
                os.remove(os.path.join(RESULTS_DIR, f))
                removed += 1
            except Exception:
                pass
    if removed:
        print(f"[CLEAN] Removed {removed} cached files")


def run_one(script: str, label: str, logf) -> bool:
    """Run a single benchmark script."""
    path = os.path.join(ROOT, "bench", script)
    cmd = [sys.executable, "-u", path]
    
    print(f"\n{'='*60}")
    print(f"[{datetime.now().strftime('%H:%M:%S')}] {label}")
    print(f"{'='*60}")
    
    logf.write(f"\n{'='*80}\n")
    logf.write(f"+ {' '.join(cmd)}\n")
    logf.write(f"started: {datetime.now().isoformat()}\n\n")
    logf.flush()
    
    if not os.path.exists(path):
        print(f"[SKIP] Not found: {path}")
        return False
    
    env = os.environ.copy()
    env["PYTHONUNBUFFERED"] = "1"
    
    start = datetime.now()
    p = subprocess.Popen(
        cmd, cwd=ROOT, env=env,
        stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
        text=True, bufsize=1,
    )
    
    for line in p.stdout:
        print(line, end="", flush=True)
        logf.write(line)
    
    rc = p.wait()
    elapsed = (datetime.now() - start).total_seconds()
    
    status = "OK" if rc == 0 else "FAIL"
    msg = f"[{status}] {label} ({elapsed:.0f}s)"
    print(msg)
    logf.write(f"\n{msg}\n")
    logf.flush()
    
    return rc == 0


def main():
    # Parse args
    args = sys.argv[1:]
    clean = "--clean" in args
    args = [a for a in args if a != "--clean"]
    
    # Filter benchmarks if specified
    if args:
        try:
            indices = {int(a) for a in args}
            selected = [(i+1, s, l) for i, (s, l) in enumerate(BENCHMARKS) if i+1 in indices]
        except ValueError:
            print("Usage: run_benchmarks.py [--clean] [1 2 3...]")
            return 1
    else:
        selected = [(i+1, s, l) for i, (s, l) in enumerate(BENCHMARKS)]
    
    if clean:
        clear_cache()
    
    os.makedirs(RESULTS_DIR, exist_ok=True)
    log_path = os.path.join(RESULTS_DIR, "benchmark_run.log")
    
    print(f"\nFogROS2-SGC Benchmark Suite")
    print(f"Running {len(selected)} benchmark(s)")
    print(f"Log: {log_path}\n")
    
    ok = fail = 0
    
    with open(log_path, "w") as logf:
        logf.write(f"FogROS2-SGC Benchmark Suite\n")
        logf.write(f"Started: {datetime.now().isoformat()}\n")
        logf.write(f"Benchmarks: {len(selected)}\n")
        
        for num, script, label in selected:
            if run_one(script, f"[{num}] {label}", logf):
                ok += 1
            else:
                fail += 1
        
        logf.write(f"\n{'='*80}\n")
        logf.write(f"Completed: {datetime.now().isoformat()}\n")
        logf.write(f"Results: ok={ok} failed={fail}\n")
    
    print(f"\n{'='*60}")
    print(f"DONE: {ok} passed, {fail} failed")
    
    # List outputs
    print(f"\nGenerated files:")
    for f in sorted(os.listdir(RESULTS_DIR)):
        if f.endswith(".png"):
            size = os.path.getsize(os.path.join(RESULTS_DIR, f))
            print(f"  {f} ({size//1024}KB)")
    
    return 0 if fail == 0 else 1


if __name__ == "__main__":
    raise SystemExit(main())

