#!/usr/bin/env python3
import argparse
import json
import os


def _load_payload(results_json: str) -> dict:
    with open(results_json, "r") as f:
        payload = json.load(f)
    if not isinstance(payload, dict) or not isinstance(payload.get("runs"), list):
        raise SystemExit(
            f"Unrecognized results format in {results_json} "
            f"(expected {{'meta':..., 'runs':[...]}}, got {type(payload).__name__})"
        )
    return payload


def load_service_bytes_by_fanout(results_json: str) -> dict[int, dict[int, dict[str, dict[str, int]]]]:
    """
    Returns:
      { fanout: { num_listeners: { service_name: {"rx": bytes, "tx": bytes} } } }
    """
    out: dict[int, dict[int, dict[str, dict[str, int]]]] = {}
    payload = _load_payload(results_json)
    for r in payload["runs"]:
        if not r.get("success"):
            continue
        fanout = int(r.get("fanout", -1))
        n = int(r.get("num_listeners"))
        containers = r.get("containers", {}) or {}

        by_svc: dict[str, dict[str, int]] = {}
        for _cname, meta in containers.items():
            svc = (meta or {}).get("service") or "unknown"
            by_svc.setdefault(svc, {"rx": 0, "tx": 0})
            by_svc[svc]["rx"] += int((meta or {}).get("rx", 0))
            by_svc[svc]["tx"] += int((meta or {}).get("tx", 0))

        # include top-level totals too, since they're useful to plot
        by_svc.setdefault("__total__", {"rx": 0, "tx": 0})
        by_svc["__total__"]["rx"] = int(r.get("total_rx", 0))
        by_svc["__total__"]["tx"] = int(r.get("total_tx", 0))

        out.setdefault(fanout, {})[n] = by_svc
    return out

def bytes_to_mbps(num_bytes: int, measure_secs: float) -> float:
    return (num_bytes * 8.0) / (measure_secs * 1e6) if measure_secs > 0 else 0.0


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--results", required=True, help="Path to bench/results/bandwidth_results.json")
    ap.add_argument(
        "--service",
        default="talker",
        help="Which service to plot: talker, proxy, rib, listener, dashboard, signal, __total__",
    )
    ap.add_argument("--dir", choices=["tx", "rx"], default="tx", help="Plot TX or RX")
    ap.add_argument("--out", default=None, help="Output PNG path (default: alongside hierarchical json)")
    ap.add_argument("--show", action="store_true", help="Show interactive plot window")
    args = ap.parse_args()

    import matplotlib.pyplot as plt

    payload = _load_payload(args.results)
    meta = payload.get("meta", {}) or {}
    measure_secs = float(meta.get("measure_secs", 30.0))

    by_fanout = load_service_bytes_by_fanout(args.results)
    fanouts = sorted(k for k in by_fanout.keys() if k != -1)
    if not fanouts:
        raise SystemExit("No successful runs found in results JSON.")

    base_for_out = os.path.splitext(args.results)[0]

    def series(runs: dict[int, dict[str, dict[str, int]]]) -> dict[int, float]:
        out: dict[int, float] = {}
        for n, by_svc in runs.items():
            b = int((by_svc.get(args.service, {}) or {}).get(args.dir, 0))
            out[n] = bytes_to_mbps(b, measure_secs)
        return out

    series_by_fanout = {f: series(by_fanout.get(f, {})) for f in fanouts}
    xs = sorted({x for s in series_by_fanout.values() for x in s.keys()})
    if not xs:
        raise SystemExit("No successful runs found in results JSON.")

    fig, ax = plt.subplots(figsize=(9.5, 4.8))
    x_pos = list(range(len(xs)))

    # Grouped bar chart: one bar per fanout at each listener count.
    # (If you have many fanouts, this will get crowded.)
    group_width = 0.82
    n_series = max(1, len(fanouts))
    bar_w = group_width / n_series
    base_offsets = [(i - (n_series - 1) / 2) * bar_w for i in range(n_series)]

    cmap = plt.get_cmap("tab10")
    for i, f in enumerate(fanouts):
        vals = [series_by_fanout[f].get(x, 0.0) for x in xs]
        xs_shifted = [p + base_offsets[i] for p in x_pos]
        ax.bar(xs_shifted, vals, width=bar_w, color=cmap(i % 10), label=f"fanout{f}")

    title_svc = args.service if args.service != "__total__" else "total"
    ax.set_title(f"{title_svc} {args.dir.upper()} bandwidth vs listener count")
    ax.set_xlabel("Listeners")
    ax.set_ylabel(f"{title_svc} {args.dir.upper()} (Mbps)")
    ax.set_xticks(x_pos)
    ax.set_xticklabels([str(x) for x in xs])
    ax.grid(True, axis="y", linestyle="--", linewidth=0.6, alpha=0.6)
    ax.legend()

    out = args.out
    if out is None:
        out = base_for_out + f"_all_fanouts_{title_svc}_{args.dir}_bars.png"

    os.makedirs(os.path.dirname(out) or ".", exist_ok=True)
    fig.tight_layout()
    fig.savefig(out, dpi=160)
    print(out)

    if args.show:
        plt.show()


if __name__ == "__main__":
    main()


