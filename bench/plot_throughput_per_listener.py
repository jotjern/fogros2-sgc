#!/usr/bin/env python3
import json
import os
import subprocess
import sys


RESULTS = "bench/results/system_results.json"
OUT = "bench/results/throughput_per_listener.png"


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
            "--out",
            RESULTS,
        ]
    )


def main():
    _ensure_results()
    with open(RESULTS, "r") as f:
        payload = json.load(f)

    runs = payload.get("runs", []) or []

    data = {}
    for r in runs:
        if not r.get("success"):
            continue
        fanout = int(r.get("fanout", -1))
        n = int(r.get("num_listeners", 0))
        bw = (r.get("bandwidth", {}) or {})
        per = float(bw.get("talker_tx_mbps_per_listener", 0.0) or 0.0)
        data.setdefault(fanout, {})[n] = per

    fanouts = sorted(data.keys())
    xs = sorted({n for d in data.values() for n in d.keys()})
    if not fanouts or not xs:
        raise SystemExit("No successful runs found in system_results.json (missing bandwidth data?)")

    import matplotlib.pyplot as plt

    fig, ax = plt.subplots(figsize=(9.5, 4.8))
    x_pos = list(range(len(xs)))

    group_width = 0.82
    bar_w = group_width / max(1, len(fanouts))
    offsets = [(i - (len(fanouts) - 1) / 2) * bar_w for i in range(len(fanouts))]
    cmap = plt.get_cmap("tab10")

    for i, fno in enumerate(fanouts):
        vals = [data[fno].get(n, 0.0) for n in xs]
        ax.bar([p + offsets[i] for p in x_pos], vals, width=bar_w, color=cmap(i % 10), label=f"fanout{fno}")

    ax.set_title("Publisher throughput per listener (talker TX / listeners)")
    ax.set_xlabel("Listeners")
    ax.set_ylabel("Mbps per listener")
    ax.set_xticks(x_pos)
    ax.set_xticklabels([str(x) for x in xs])
    ax.grid(True, axis="y", linestyle="--", linewidth=0.6, alpha=0.6)
    ax.legend()

    os.makedirs(os.path.dirname(OUT) or ".", exist_ok=True)
    fig.tight_layout()
    fig.savefig(OUT, dpi=160)
    print(OUT)


if __name__ == "__main__":
    main()

