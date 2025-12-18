#!/usr/bin/env python3
import argparse
import os
import subprocess
import sys
from datetime import datetime


ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
RESULTS_DIR = os.path.join(ROOT, "bench", "results")

SYSTEM_BENCH = os.path.join(ROOT, "bench", "system_benchmark.py")
SYS_RESULTS = os.path.join(ROOT, "bench", "results", "system_results.json")
SYS_RESULTS_CHURN10 = os.path.join(ROOT, "bench", "results", "system_results_churn10.json")


def ts():
    return datetime.now().strftime("%Y%m%d_%H%M%S")


PLOTS = [
    os.path.join(ROOT, "bench", "plot_throughput_per_listener.py"),
    os.path.join(ROOT, "bench", "plot_latency_per_listener.py"),
    os.path.join(ROOT, "bench", "plot_cpu_churn10.py"),
]


def run_one(cmd, logf, label):
    print(f"[RUN] {label}", flush=True)
    logf.write("\n" + ("=" * 80) + "\n")
    logf.write(f"+ {' '.join(cmd)}\n")
    logf.flush()

    # Stream stdout/stderr live to terminal, while also logging to file.
    env = os.environ.copy()
    env["PYTHONUNBUFFERED"] = "1"
    p = subprocess.Popen(
        cmd,
        cwd=ROOT,
        env=env,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
        bufsize=1,
    )
    assert p.stdout is not None
    for line in p.stdout:
        print(line, end="", flush=True)
        logf.write(line)
        logf.flush()
    rc = p.wait()

    if rc != 0:
        logf.write(f"[SKIP] failed: {label} (exit={rc})\n")
        logf.flush()
        print(f"[FAIL] {label} (exit={rc})", flush=True)
        return False

    logf.write(f"[OK] {label}\n")
    logf.flush()
    print(f"[OK] {label}", flush=True)
    return True


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--fanouts", default="3,1000")
    ap.add_argument("--listeners", default="25")
    ap.add_argument("--proxy-mode", default="auto")
    ap.add_argument("--measure-secs", type=int, default=1)
    ap.add_argument("--timeout-secs", type=int, default=240)
    args = ap.parse_args()

    os.makedirs(RESULTS_DIR, exist_ok=True)
    log_path = os.path.join(RESULTS_DIR, f"plot_run_{ts()}.log")
    ok = 0
    fail = 0

    with open(log_path, "w") as logf:
        logf.write(f"run at {datetime.now().isoformat()}\n")
        print(f"[LOG] {log_path}", flush=True)

        # Always generate the JSON inputs first (skip-but-log on failure).
        bench_cmd = [
            sys.executable,
            "-u",
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
        if run_one(bench_cmd, logf, "system_benchmark (normal)"):
            ok += 1
        else:
            fail += 1

        churn_cmd = [
            sys.executable,
            "-u",
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
        if run_one(churn_cmd, logf, "system_benchmark (10% churn)"):
            ok += 1
        else:
            fail += 1

        for path in PLOTS:
            if not os.path.exists(path):
                logf.write(f"[SKIP] missing plot script: {path}\n")
                fail += 1
                print(f"[SKIP] missing plot script: {os.path.basename(path)}", flush=True)
                continue
            if run_one([sys.executable, "-u", path], logf, os.path.basename(path)):
                ok += 1
            else:
                fail += 1

        logf.write("\n")
        logf.write(f"done: ok={ok} failed={fail}\n")

    print(f"[DONE] ok={ok} failed={fail}", flush=True)
    print(log_path, flush=True)
    return 0 if fail == 0 else 1


if __name__ == "__main__":
    raise SystemExit(main())

