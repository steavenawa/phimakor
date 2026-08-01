"""Generate a playable chart entirely in memory and save it to disk.

Shows the full headless pipeline: build the RPE JSON in Python, parse it
with `RPEChart.from_json`, evaluate it with `Chart.from_rpe_chart` +
`state_at`, then open the result as an editable `Editor` document.

Run:
    python example/generate_chart.py [OUT_DIR]
"""

import json
import os
import sys
from fractions import Fraction

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))

import phimakor as pk

BPM = 150.0


def triple(beat):
    """Convert a float beat to RPE's [i, n, d] triple form (i + n/d)."""
    f = Fraction(float(beat)).limit_denominator(1000)
    i = f.numerator // f.denominator
    n = f.numerator % f.denominator
    return [i, n, f.denominator]


def ev(start, end, b0, b1, easing=1):
    return {"start": start, "end": end,
            "startTime": triple(b0), "endTime": triple(b1),
            "easingType": easing}


def note(kind, beat, x, above=True, hold=0.0):
    return {"type": kind, "above": 1 if above else 0,
            "startTime": triple(beat), "endTime": triple(beat + hold),
            "positionX": x, "yOffset": 0.0, "alpha": 255, "speed": 1.0}


def build_chart_json():
    """A 64-beat demo: a drifting main line and a floating sky line."""
    main_notes = []
    for i in range(16):
        beat = 4.0 * i + 2.0
        kind = 3 if i % 4 == 2 else 1  # hold every 4th
        main_notes.append(note(kind, beat, 0.1 * (i % 5), hold=1.0 if kind == 3 else 0.0))

    sky_notes = []
    for i in range(8):
        sky_notes.append(note(2, 8.0 * i, 0.5, above=i % 2 == 0))  # drags

    return {
        "META": {"offset": 50, "RPEVersion": 160},
        "BPMList": [{"bpm": BPM, "startTime": [0, 0, 1]}],
        "judgeLineList": [
            {
                "Name": "Main",
                "Texture": "line.png",
                "father": -1,
                "isCover": 0,
                "eventLayers": [{
                    "alphaEvents": [ev(255, 255, 0, 64)],
                    "moveXEvents": [ev(-300.0, 300.0, 0, 32), ev(300.0, -250.0, 32, 64)],
                    "moveYEvents": [ev(0.0, 150.0, 0, 64)],
                    "rotateEvents": [ev(0, 30, 0, 32, easing=2), ev(30, 0, 32, 64, easing=2)],
                    "speedEvents": [ev(1.0, 1.0, 0, 64)],
                }],
                "notes": main_notes,
            },
            {
                "Name": "Sky",
                "Texture": "line.png",
                "father": 0,
                "isCover": 0,
                "eventLayers": [{
                    "alphaEvents": [ev(200, 80, 0, 64)],
                    "moveYEvents": [ev(350.0, 500.0, 0, 64)],
                }],
                "notes": sky_notes,
            },
        ],
    }


def main():
    out_dir = sys.argv[1] if len(sys.argv) > 1 else os.path.join(
        os.path.dirname(os.path.abspath(__file__)), "generated_chart")
    os.makedirs(out_dir, exist_ok=True)

    rpe = pk.RPEChart.from_json(json.dumps(build_chart_json()))
    chart = pk.Chart.from_rpe_chart(rpe)
    print(f"in-memory chart: {chart.line_count()} lines, "
          f"{chart.note_count()} notes, {chart.duration():.1f}s, "
          f"max combo {chart.max_combo()}")

    st = chart.state_at(16.0)
    line0 = st.lines[0]
    print(f"at t=16s: Main line at {[round(v, 2) for v in line0.position]}, "
          f"rotation {line0.rotation * 57.2958:.0f} deg, alpha {line0.alpha:.0f}")

    with open(os.path.join(out_dir, "chart.json"), "w", encoding="utf-8") as f:
        f.write(rpe.to_json())
    with open(os.path.join(out_dir, "info.json"), "w", encoding="utf-8") as f:
        json.dump({"chart": "chart.json", "name": "Generated Demo",
                   "composer": "phimakor", "level": "IN", "difficulty": 12.0,
                   "offset": 0.0}, f, indent=2)
    print(f"saved chart directory: {out_dir}")

    # Reopen as an editable document and mutate it
    doc = pk.Editor.open(out_dir)
    doc.add_note(line=0, note=pk.Note(kind=4, start_beat=30.0, position_x=0.5))
    doc.add_bpm(180.0, 32.0)
    print(f"edited: {doc.chart().judge_line(0).note_count()} notes on Main, "
          f"{len(doc.bpm_list())} BPM segments")
    doc.save()
    print(f"saved with edits -> {os.path.join(out_dir, 'chart.json')}")


if __name__ == "__main__":
    main()
