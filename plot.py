"""
Plot latency for 1–10 nodes with base vs non-base scenarios.
The styling follows a minimalist poster-like aesthetic: white background,
thin blue outlines, light gridlines, and restrained palette.
"""

import matplotlib.pyplot as plt
from matplotlib.patches import Rectangle


def plot_latency():
    nodes = list(range(1, 11))
    # Sample latency data (ms): mild increase with more nodes
    non_base_latency = [2.40,4.81,7.22,9.59,9.40,9.59,9.41,9.37]# hierarchy
    base_latency = [2.32,4.90,7.22,9.41,11.6,14.5,17.4,19.6]# flat
    # base_latency = [20, 21, 22, 23, 24, 25, 26, 27, 28, 29]
    # non_base_latency = [22, 23, 24, 25, 26, 27, 28, 29, 30, 31]

    plt.rcParams.update(
        {
            "font.family": "DejaVu Sans",
            "axes.spines.top": False,
            "axes.spines.right": False,
        }
    )

    fig, ax = plt.subplots(figsize=(8, 4.5), facecolor="white")
    ax.set_facecolor("white")

    # Draw a thin blue border around the plotting area (in axes coords).
    border = Rectangle(
        (-0.02, -0.02),
        1.04,
        1.04,
        transform=ax.transAxes,
        fill=False,
        lw=1.0,
        edgecolor="#1f4ba7",
    )
    ax.add_patch(border)

    # Ensure data lengths line up; truncate to the shortest sequence if needed.
    n = min(len(nodes), len(base_latency), len(non_base_latency))
    nodes = nodes[:n]
    base_latency = base_latency[:n]
    non_base_latency = non_base_latency[:n]

    indices = range(n)
    width = 0.38

    base_bars = ax.bar(
        [i - width / 2 for i in indices],
        base_latency,
        width=width,
        color="#1f77b4",
        edgecolor="#1f4ba7",
        linewidth=0.8,
        label="Flat",
    )
    non_base_bars = ax.bar(
        [i + width / 2 for i in indices],
        non_base_latency,
        width=width,
        color="#c04040",
        edgecolor="#a23232",
        linewidth=0.8,
        label="Hierarchical",
    )

    ax.set_xlabel("Nodes", color="#1f4ba7")
    ax.set_ylabel("Bandwidth usage (Mb/s)", color="#1f4ba7")
    ax.set_xticks(indices)
    ax.set_xticklabels(nodes, color="#222222")
    ax.tick_params(axis="y", colors="#222222")
    ax.tick_params(axis="x", colors="#222222")

    # Minimalist grid
    ax.grid(axis="y", color="#e6e6e6", linestyle="-", linewidth=0.8)
    ax.set_axisbelow(True)

    # Left/bottom spines in subtle blue/neutral
    for spine in ["left", "bottom"]:
        ax.spines[spine].set_color("#1f4ba7")
        ax.spines[spine].set_linewidth(1.0)

    # Title
    ax.set_title(
        "Upload bandwidth usage vs. Number of Nodes",
        color="#1f4ba7",
        fontsize=14,
        pad=14,
    )

    # Compact legend inside plot with thin blue border
    legend = ax.legend(
        frameon=True,
        loc="upper left",
        bbox_to_anchor=(0.02, 0.98),
        fontsize=9,
    )
    legend.get_frame().set_edgecolor("#1f4ba7")
    legend.get_frame().set_linewidth(0.8)
    legend.get_frame().set_facecolor("white")

    fig.tight_layout()
    plt.show()


if __name__ == "__main__":
    plot_latency()
