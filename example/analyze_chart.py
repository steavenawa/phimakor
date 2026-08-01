"""Analyze a chart: structure, stats, and a note-density curve.

Loads either a chart directory (info.json + chart.json) or a bare RPE JSON
file, then prints the structure and — with `--plot` — draws the cumulative
hit/combo curve and per-second note density.

Run:
    python example/analyze_chart.py <chart_dir_or_json> [--plot]
"""

import argparse
import json
import os
import sys

import numpy as np

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))

import phimakor as pk


def load(path):
    """Return (ChartInfo-like dict, RPEChart) from a dir or a .json file."""
    if os.path.isdir(path):
        doc = pk.Editor.open(path)
        info = {k: getattr(doc.info(), k) for k in
                ("name", "composer", "level", "difficulty", "charter", "offset")}
        return info, doc.chart()
    with open(path, encoding="utf-8") as f:
        return {}, pk.RPEChart.from_json(f.read())


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("chart", help="chart directory or RPE .json file")
    parser.add_argument("--plot", action="store_true", help="draw density curve")
    args = parser.parse_args()

    info, rpe = load(args.chart)
    print(f"Song: {info.get('name', '?')} | {info.get('composer', '?')} | "
          f"{info.get('level', '?')} {info.get('difficulty', '?')}")

    chart = pk.Chart.from_rpe_chart(rpe)
    print(f"duration {chart.duration():.2f}s | {chart.line_count()} lines | "
          f"{chart.note_count()} notes | max combo {chart.max_combo()}")

    for i in range(chart.line_count()):
        name = chart.line_name(i) or f"Line {i}"
        print(f"  line {i}: {name!r}")

    if args.plot:
        ts = np.arange(0.0, chart.duration(), 0.05)
        hits = [chart.hits_before(t) for t in ts]
        density = np.diff(hits) / np.diff(ts)

        import matplotlib.pyplot as plt

        fig, (ax1, ax2) = plt.subplots(2, 1, figsize=(9, 7), sharex=True)
        ax1.plot(ts, hits, color="#1f77b4")
        ax1.set_ylabel("cumulative hits / combo")
        ax1.set_title(f"{info.get('name', 'chart')} — density analysis")
        ax2.bar(ts[:-1], density, width=0.05, color="#2ca02c", alpha=0.7)
        ax2.set_xlabel("time (s)")
        ax2.set_ylabel("notes / second")
        plt.tight_layout()
        plt.show()


if __name__ == "__main__":
    main()
