"""Draw the Canton Tower (广州塔) using Phigros judge lines.

The trick: a judge line carries moveX/moveY events, so it traces a path on
the playfield over time. Two lines walk up the tower silhouette (one per
side) while a third spirals around the body — `Chart.from_rpe_chart` +
`state_at` sample every line position headlessly, matplotlib draws the
trajectories. No chart directory, no GPU: the whole pipeline is in memory.

Run:
    python example/tower_of_canton.py [--save-dir DIR]
"""

import argparse
import json
import os
import sys
from fractions import Fraction

import numpy as np

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))

import phimakor as pk

BPM = 120.0
BEATS = 24  # tower body is drawn in 24 beats = 12 s at 120 BPM
H = 600.0  # tower height, arbitrary units
MAX_W = 300.0  # half-width scale


def triple(beat):
    """Convert a float beat to RPE's [i, n, d] triple form (i + n/d)."""
    f = Fraction(float(beat)).limit_denominator(1000)
    i = f.numerator // f.denominator
    n = f.numerator % f.denominator
    return [i, n, f.denominator]


def events_from_kf(kfs):
    """[(beat, value)] -> linear RPE events between consecutive keyframes."""
    out = []
    for (b0, v0), (b1, v1) in zip(kfs, kfs[1:]):
        out.append({
            "start": float(v0), "end": float(v1),
            "startTime": triple(b0), "endTime": triple(b1),
            "easingType": 1,
        })
    return out

# (normalized height, half-width) silhouette keyframes of the tower
# body — wide base, tight waist, flared top, then the thin antenna.
SILHOUETTE = [
    (0.00, 0.26),
    (0.12, 0.20),
    (0.30, 0.12),
    (0.55, 0.20),
    (0.70, 0.22),
    (0.90, 0.10),
    (1.00, 0.02),
    (1.12, 0.008),  # antenna
]


def silhouette_kfs(sign):
    """Keyframes (beat, x, y) for one side of the tower body."""
    n = len(SILHOUETTE)
    kfs = []
    for i, (h, w) in enumerate(SILHOUETTE):
        beat = i * (BEATS / (n - 1))
        kfs.append((beat, sign * w * MAX_W, h * H))
    return kfs


def split_kfs(kfs):
    """Split (beat, x, y) keyframes into separate (beat, x) and (beat, y) lists."""
    kfx = [(b, x) for b, x, _ in kfs]
    kfy = [(b, y) for b, _, y in kfs]
    return kfx, kfy


def spiral_kfs(turns=3, points_per_turn=16):
    """Keyframes (beat, x, y) spiralling up the tower body."""
    total = turns * points_per_turn
    kfx, kfy = [], []
    for i in range(total + 1):
        t = i / total
        ang = t * turns * 2 * np.pi
        kfx.append((t * BEATS, 0.16 * MAX_W * np.cos(ang)))
        kfy.append((t * BEATS, (0.05 + 0.9 * t) * H))
    return kfx, kfy


def line_json(name, kf_x, kf_y):
    return {
        "Name": name,
        "Texture": "line.png",
        "father": -1,
        "isCover": 0,
        "eventLayers": [{
            "moveXEvents": events_from_kf(kf_x),
            "moveYEvents": events_from_kf(kf_y),
        }],
        "notes": [],
    }


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--save-dir", default=None, help="save tower.png instead of showing")
    args = parser.parse_args()

    rpe = pk.RPEChart.from_json(json.dumps({
        "META": {"offset": 0, "RPEVersion": 160},
        "BPMList": [{"bpm": BPM, "startTime": [0, 0, 1]}],
        "judgeLineList": [
            line_json("Tower Left", *split_kfs(silhouette_kfs(-1))),
            line_json("Tower Right", *split_kfs(silhouette_kfs(+1))),
            line_json("Spiral", *spiral_kfs()),
        ],
    }))

    chart = pk.Chart.from_rpe_chart(rpe)
    print(f"chart duration: {chart.duration():.2f}s, lines: {chart.line_count()}")

    # The engine normalizes the playfield (x roughly in [-1, 1]); scale the
    # x axis up for a readable silhouette — proportions stay true to the chart.
    X_EXAG = 3.0

    ts = np.arange(0.0, chart.duration(), 0.02)
    traces = [[] for _ in range(3)]
    for t in ts:
        st = chart.state_at(t)
        for i, ls in enumerate(st.lines):
            traces[i].append(ls.position)

    import matplotlib.pyplot as plt

    fig, ax = plt.subplots(figsize=(6, 10))
    colors = ["#1f77b4", "#ff7f0e", "#2ca02c"]
    names = ["left silhouette", "right silhouette", "spiral"]
    for i, name in enumerate(names):
        xs = [p[0] * X_EXAG for p in traces[i]]
        ys = [p[1] for p in traces[i]]
        ax.plot(xs, ys, color=colors[i], lw=2, label=f"judge line {i} — {name}")

    ax.set_aspect("equal")
    ax.set_title("Canton Tower, drawn by Phigros judge lines")
    ax.set_xlabel("moveX (normalized, x3 for readability)")
    ax.set_ylabel("moveY (normalized)")
    ax.legend(loc="upper right")

    if args.save_dir:
        os.makedirs(args.save_dir, exist_ok=True)
        path = os.path.join(args.save_dir, "tower.png")
        fig.savefig(path, dpi=120)
        print(f"saved: {path}")
    else:
        plt.show()


if __name__ == "__main__":
    main()
