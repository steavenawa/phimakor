# Examples

Demo scripts for the `phimakor` Python API (the `import phimakor` wheel).
Everything here is headless — no GUI, no absolute paths, no chart directory
required unless noted. `format_roundtrip.py`, `editor_ops.py` and
`extra_effects.py` are zero-dependency (stdlib + `phimakor` only) and print an
"all checks passed" line when they succeed.

## Setup

Build and install the Python module first (from the repo root):

```sh
pip install maturin
python -m maturin build --release
pip install target/wheels/phimakor-*.whl
```

Dependencies for the plotting scripts (`tower_of_canton.py` always,
`analyze_chart.py` only with `--plot`):

```sh
pip install numpy matplotlib
```

## Scripts

| Script | What it does |
|--------|--------------|
| `tower_of_canton.py` | Draws the Canton Tower (广州塔) with Phigros judge lines. Two lines walk the tower silhouette, one spirals around the body; `Chart.from_rpe_chart` + `state_at` sample every line's trajectory headlessly and matplotlib renders it. |
| `generate_chart.py` | Generates a playable chart purely in memory (RPE JSON -> `RPEChart.from_json` -> evaluate with `Chart.from_rpe_chart`), writes a chart directory to disk, then reopens it as an `Editor` document, mutates it (add note / BPM segment) and saves. |
| `analyze_chart.py` | Loads a chart directory or bare RPE JSON and prints structure + stats (duration, line/note counts, max combo), and with `--plot` draws the cumulative-hit curve and per-second note density. |
| `format_roundtrip.py` | Probes and parses charts in all four formats — `detect_format_bytes` / `parse_chart_bytes` on PEC / PGR / PSS / PMK samples (PMK via an embedded fixture, read-only), `RPEChart.to_pss` / `from_pss` round-trip, file-based `detect_format` / `parse_chart`, plus `Chart.hits_before` / `Chart.textures` and `ChartInfo.from_info_txt`. |
| `editor_ops.py` | Full `Editor` session chain on a throwaway chart dir: `add_line` / `add_note` / `add_event` / `add_bpm`, `split_line` + `bind_lines` with undo/redo verification, point edits (`replace_note` / `remove_note` / `remove_event` / `replace_bpm` / `remove_bpm` / `remove_line`), `is_dirty`, and the `save_background` + `flush` + `save` disk-write chain. |
| `extra_effects.py` | Parses an `extra.json` (effects + BPM overrides) with `ExtraRoot.parse` and resolves active effects at several beats with `evaluate` — priority ordering, global flag, flat and keyframed uniforms (`EvalEffect`). |

## Run

```sh
python example/tower_of_canton.py            # shows the tower interactively
python example/tower_of_canton.py --save-dir example   # or save tower.png
python example/generate_chart.py
python example/analyze_chart.py example/generated_chart --plot
python example/format_roundtrip.py           # self-checking, prints "all checks passed"
python example/editor_ops.py                 # self-checking, prints "all checks passed"
python example/extra_effects.py              # self-checking, prints "all checks passed"
```

The self-checking scripts are also safe to pipe through CI: they exit nonzero
on the first failed assertion.
