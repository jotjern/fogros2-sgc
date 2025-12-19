#!/usr/bin/env python3
import os
import subprocess
import sys
from datetime import datetime


ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
RESULTS_DIR = os.path.join(ROOT, "bench", "results")

SCRIPTS = [
    os.path.join(ROOT, "bench", "benchmark1_compare.py"),
    os.path.join(ROOT, "bench", "benchmark2_join_latency.py"),
    os.path.join(ROOT, "bench", "benchmark3_storm.py"),
    os.path.join(ROOT, "bench", "benchmark4_jitter.py"),
    os.path.join(ROOT, "bench", "benchmark5_proxy_cpu_churn.py"),
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
    os.makedirs(RESULTS_DIR, exist_ok=True)
    log_path = os.path.join(RESULTS_DIR, "run_all.log")
    ok = 0
    fail = 0

    with open(log_path, "w") as logf:
        logf.write(f"run at {datetime.now().isoformat()}\n")
        print(f"[LOG] {log_path}", flush=True)

        for path in SCRIPTS:
            if not os.path.exists(path):
                logf.write(f"[SKIP] missing script: {path}\n")
                fail += 1
                print(f"[SKIP] missing script: {os.path.basename(path)}", flush=True)
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

