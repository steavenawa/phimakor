# Examples

Demo scripts for the `phimakor` Python API (the `import phimakor` wheel).
Everything here is headless — no GUI, no chart directory required unless noted.

## Setup

Build and install the Python module first (from the repo root):

```sh
pip install maturin
python -m maturin build --release
pip install target/wheels/phimakor-*.whl
```

Dependencies for the plotting scripts:

```sh
pip install numpy matplotlib
```

## Scripts

| Script | What it does |
|--------|--------------|
| `tower_of_canton.py` | Draws the Canton Tower (广州塔) with Phigros judge lines. Two lines walk the tower silhouette, one spirals around the body; `Chart.from_rpe_chart` + `state_at` sample every line's trajectory headlessly and matplotlib renders it. |
| `generate_chart.py` | Generates a playable chart purely in memory (RPE JSON -> `RPEChart.from_json` -> evaluate with `Chart.from_rpe_chart`), writes a chart directory to disk, then reopens it as an `Editor` document, mutates it (add note / BPM segment) and saves. |
| `analyze_chart.py` | Loads a chart directory or bare RPE JSON and prints structure + stats (duration, line/note counts, max combo), and with `--plot` draws the cumulative-hit curve and per-second note density. |

## Run

```sh
python example/tower_of_canton.py            # shows the tower interactively
python example/tower_of_canton.py --save-dir example   # or save tower.png
python example/generate_chart.py
python example/analyze_chart.py example/generated_chart --plot
```
