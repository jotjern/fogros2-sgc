#!/usr/bin/env python3
import json
import os
import subprocess
import sys


RESULTS = "bench/results/system_results_churn10.json"
OUT = "bench/results/cpu_churn10.png"


def _ensure_results():
    if os.path.exists(RESULTS):
        return
    subprocess.check_call(
        [
            sys.executable,
            "bench/system_benchmark.py",
            "--fanouts",
            "3,1000",
            "--listeners",
            "50",
            "--proxy-mode",
            "auto",
            "--chaos-restart",
            "--chaos-percent",
            "10",
            "--out",
            RESULTS,
        ]
    )


def main():
    _ensure_results()
    with open(RESULTS, "r") as f:
        payload = json.load(f)
    runs = payload.get("runs", []) or []

    # fanout -> service -> avg_cpu
    data = {}
    for r in runs:
        if not r.get("success"):
            continue
        fanout = int(r.get("fanout", -1))
        svc_avg = ((r.get("cpu", {}) or {}).get("service_avg_cpu_percent", {}) or {})
        data[fanout] = {str(k): float(v or 0.0) for k, v in svc_avg.items()}

    fanouts = sorted(data.keys())
    services = sorted({s for d in data.values() for s in d.keys() if s})
    if not fanouts or not services:
        raise SystemExit("No successful runs found in system_results_churn10.json")

    import matplotlib.pyplot as plt

    fig, ax = plt.subplots(figsize=(10.5, 5.2))
    x_pos = list(range(len(services)))

    group_width = 0.82
    bar_w = group_width / max(1, len(fanouts))
    offsets = [(i - (len(fanouts) - 1) / 2) * bar_w for i in range(len(fanouts))]
    cmap = plt.get_cmap("tab10")

    for i, fno in enumerate(fanouts):
        vals = [data[fno].get(s, 0.0) for s in services]
        ax.bar([p + offsets[i] for p in x_pos], vals, width=bar_w, color=cmap(i % 10), label=f"fanout{fno}")

    ax.set_title("CPU usage per service (10% churn)")
    ax.set_xlabel("Service")
    ax.set_ylabel("Avg CPU (%)")
    ax.set_xticks(x_pos)
    ax.set_xticklabels(services, rotation=30, ha="right")
    ax.grid(True, axis="y", linestyle="--", linewidth=0.6, alpha=0.6)
    ax.legend()

    os.makedirs(os.path.dirname(OUT) or ".", exist_ok=True)
    fig.tight_layout()
    fig.savefig(OUT, dpi=160)
    print(OUT)


if __name__ == "__main__":
    main()

