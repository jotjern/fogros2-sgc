#!/usr/bin/env python3
import argparse
import os
import subprocess
import sys


ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))

SYS_RESULTS = os.path.join(ROOT, "bench", "results", "system_results.json")
SYS_RESULTS_CHURN10 = os.path.join(ROOT, "bench", "results", "system_results_churn10.json")

PLOT_THROUGHPUT = os.path.join(ROOT, "bench", "plot_throughput_per_listener.py")
PLOT_LATENCY = os.path.join(ROOT, "bench", "plot_latency_per_listener.py")
PLOT_CPU_CHURN10 = os.path.join(ROOT, "bench", "plot_cpu_churn10.py")

SYSTEM_BENCH = os.path.join(ROOT, "bench", "system_benchmark.py")


def run(cmd):
    print("+ " + " ".join(cmd), flush=True)
    subprocess.check_call(cmd, cwd=ROOT)


def rm_if_exists(p):
    try:
        os.remove(p)
    except FileNotFoundError:
        pass


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--fanouts", default="3,1000")
    ap.add_argument("--listeners", default="50")
    ap.add_argument("--proxy-mode", default="auto")
    ap.add_argument("--measure-secs", type=int, default=30)
    ap.add_argument("--timeout-secs", type=int, default=240)
    ap.add_argument("--force", action="store_true", help="Delete cached json/png outputs before running.")
    args = ap.parse_args()

    if args.force:
        rm_if_exists(SYS_RESULTS)
        rm_if_exists(SYS_RESULTS_CHURN10)
        rm_if_exists(os.path.join(ROOT, "bench", "results", "throughput_per_listener.png"))
        rm_if_exists(os.path.join(ROOT, "bench", "results", "avg_latency_per_listener.png"))
        rm_if_exists(os.path.join(ROOT, "bench", "results", "cpu_churn10.png"))

    # Benchmarks (write JSON)
    run(
        [
            sys.executable,
            SYSTEM_BENCH,
            "--fanouts",
            args.fanouts,
            "--listeners",
            args.listeners,
            "--proxy-mode",
            args.proxy_mode,
            "--measure-secs",
            str(args.measure_secs),
            "--timeout-secs",
            str(args.timeout_secs),
            "--out",
            SYS_RESULTS,
        ]
    )

    run(
        [
            sys.executable,
            SYSTEM_BENCH,
            "--fanouts",
            args.fanouts,
            "--listeners",
            args.listeners,
            "--proxy-mode",
            args.proxy_mode,
            "--measure-secs",
            str(args.measure_secs),
            "--timeout-secs",
            str(args.timeout_secs),
            "--chaos-restart",
            "--chaos-percent",
            "10",
            "--out",
            SYS_RESULTS_CHURN10,
        ]
    )

    # Plots (reuse JSONs)
    run([sys.executable, PLOT_THROUGHPUT])
    run([sys.executable, PLOT_LATENCY])
    run([sys.executable, PLOT_CPU_CHURN10])


if __name__ == "__main__":
    main()

