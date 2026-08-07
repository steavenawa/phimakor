"""Exercise the Editor session API end to end: line/note/event/BPM editing,
split/bind, undo/redo, and the background-save chain.

Runs headlessly against a throwaway chart directory (built in a temp dir, so
nothing in the repo is touched). Every step asserts the observable state, so
the script doubles as a smoke test for `phimakor.Editor`.

Covers: open, add_line, add_note, add_event, add_bpm, split_line, bind_lines,
undo, redo, can_undo, can_redo, is_dirty, replace_note, remove_note,
remove_event, replace_bpm, remove_bpm, remove_line, save_background, flush,
save, chart, info, bpm_list.

Dependencies: only `phimakor`. No GUI, no absolute paths.

Run:
    python example/editor_ops.py
"""

import json
import os
import sys
import tempfile

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))

import phimakor as pk


def build_chart_json():
    """Two lines: Main with a tap at beat 2 and a hold at beat 6; Sky with a
    drag at beat 4. One BPM segment (120 @ 0)."""
    return {
        "META": {"offset": 0, "RPEVersion": 160},
        "BPMList": [{"bpm": 120.0, "startTime": [0, 0, 1]}],
        "judgeLineList": [
            {
                "Name": "Main",
                "Texture": "line.png",
                "father": -1,
                "isCover": 0,
                "eventLayers": [{
                    "alphaEvents": [], "moveXEvents": [], "moveYEvents": [],
                    "rotateEvents": [], "speedEvents": [],
                }],
                "notes": [
                    {"type": 1, "above": 1, "startTime": [0, 2, 1], "endTime": [0, 2, 1],
                     "positionX": 0.0, "yOffset": 0.0, "alpha": 255, "speed": 1.0},
                    {"type": 3, "above": 1, "startTime": [0, 6, 1], "endTime": [0, 8, 1],
                     "positionX": 0.1, "yOffset": 0.0, "alpha": 255, "speed": 1.0},
                ],
            },
            {
                "Name": "Sky",
                "Texture": "line.png",
                "father": 0,
                "isCover": 0,
                "eventLayers": [{
                    "alphaEvents": [], "moveXEvents": [], "moveYEvents": [],
                    "rotateEvents": [], "speedEvents": [],
                }],
                "notes": [
                    {"type": 2, "above": 1, "startTime": [0, 4, 1], "endTime": [0, 4, 1],
                     "positionX": 0.5, "yOffset": 0.0, "alpha": 255, "speed": 1.0},
                ],
            },
        ],
    }


def line_notes(doc, idx):
    return doc.chart().judge_line(idx).note_count()


def check(tag, cond, detail=""):
    assert cond, f"FAIL [{tag}] {detail}"
    print(f"  ok [{tag}] {detail}")


def main():
    print("== editor_ops: Editor add/split/bind/undo/redo/save chain ==")

    with tempfile.TemporaryDirectory(prefix="phimakor_editor_") as tmp:
        with open(os.path.join(tmp, "chart.json"), "w", encoding="utf-8") as f:
            f.write(pk.RPEChart.from_json(json.dumps(build_chart_json())).to_json())
        with open(os.path.join(tmp, "info.json"), "w", encoding="utf-8") as f:
            json.dump({"chart": "chart.json", "name": "Editor Ops Demo",
                       "composer": "phimakor", "level": "IN", "difficulty": 12.0,
                       "offset": 0.0}, f, indent=2)

        doc = pk.Editor.open(tmp)
        check("open", doc.chart().judge_line_count() == 2
              and line_notes(doc, 0) == 2 and line_notes(doc, 1) == 1, "2 lines, Main=2 notes")
        check("info", doc.info().name == "Editor Ops Demo", doc.info().name)
        check("clean", not doc.is_dirty(), "fresh document is clean")

        # --- structure editing -------------------------------------------------
        idx = doc.add_line("Bonus", "line.png")
        check("add_line", idx == 2 and doc.chart().judge_line_count() == 3,
              f"Bonus at index {idx}")
        doc.add_note(line=0, note=pk.Note(kind=1, start_beat=4.0, position_x=0.1))
        check("add_note", line_notes(doc, 0) == 3, "Main now 3 notes (sorted insert)")
        doc.add_event(line=0, layer=0, kind="alpha",
                      event=pk.Event(start=255.0, end=255.0, start_beat=0.0,
                                     end_beat=8.0, easing_type=1))
        check("add_event", len(doc.chart().judge_line(0).events("alpha")) == 1,
              "Main has 1 alpha keyframe")
        doc.add_bpm(180.0, 4.0)
        check("add_bpm", doc.bpm_list() == [(120.0, 0.0), (180.0, 4.0)],
              str(doc.bpm_list()))
        check("dirty", doc.is_dirty(), "edits set the dirty flag")

        # --- split + bind ------------------------------------------------------
        new_idx = doc.split_line(0, 5.0)
        check("split_line", new_idx == 1 and doc.chart().judge_line_count() == 4,
              f"Main split at beat 5 -> line {new_idx}")
        check("split moved notes",
              line_notes(doc, 0) == 2 and line_notes(doc, 1) == 1,
              "notes <5.0 stay on Main, beat-6 hold moves to the split line")
        check("split name", doc.chart().judge_line(1).name == "Main (split)",
              doc.chart().judge_line(1).name)

        doc.bind_lines(0, 1)
        check("bind_lines", doc.chart().judge_line_count() == 3
              and line_notes(doc, 0) == 3, "split line merged back into Main")

        # --- undo / redo ---------------------------------------------------------
        check("undo bind", doc.undo() and doc.chart().judge_line_count() == 4,
              "undo restores the split line")
        check("undo split", doc.undo() and doc.chart().judge_line_count() == 3
              and line_notes(doc, 0) == 3, "undo restores the original Main")
        check("redo split", doc.redo() and doc.chart().judge_line_count() == 4,
              "redo re-applies the split")
        check("redo bind", doc.redo() and doc.chart().judge_line_count() == 3,
              "redo re-merges the lines")
        check("can_undo", doc.can_undo(), "stack not empty")
        check("can_redo empty", not doc.can_redo(), "redo stack drained")
        check("redo noop", not doc.redo(), "redo() on empty stack returns False")

        # --- point edits ---------------------------------------------------------
        old = doc.replace_note(line=0, index=0,
                               note=pk.Note(kind=2, start_beat=2.5, position_x=0.2))
        check("replace_note", old.kind == 1
              and doc.chart().judge_line(0).notes()[0].kind == 2,
              "slot kept, kind 1 -> 2")
        before_events = len(doc.chart().judge_line(0).events("alpha"))
        removed = doc.remove_event(line=0, layer=0, kind="alpha", index=0)
        after_events = len(doc.chart().judge_line(0).events("alpha"))
        check("remove_event", after_events == before_events - 1,
              f"{before_events} -> {after_events}, removed {removed.start}->{removed.end}")
        gone = doc.remove_note(line=0, index=0)
        check("remove_note", gone.kind == 2 and line_notes(doc, 0) == 2,
              f"removed kind {gone.kind}")
        doc.replace_bpm(0, 90.0, 0.0)
        doc.remove_bpm(1)
        check("bpm replace/remove", doc.bpm_list() == [(90.0, 0.0)], str(doc.bpm_list()))
        doc.remove_line(1)
        check("remove_line", doc.chart().judge_line_count() == 2
              and doc.chart().judge_line(1).name == "Bonus", "Sky removed")

        # --- background save chain -------------------------------------------------
        doc.save_background()
        doc.flush()
        check("flush clean", not doc.is_dirty(), "background save acknowledged")
        on_disk = pk.RPEChart.from_json(
            open(os.path.join(tmp, "chart.json"), encoding="utf-8").read())
        check("disk matches", on_disk.judge_line_count() == 2
              and on_disk.judge_line(0).note_count() == 2
              and on_disk.bpm_list() == [(90.0, 0.0)],
              "chart.json reflects the edits")

        # --- explicit save ----------------------------------------------------------
        doc.add_note(line=0, note=pk.Note(kind=4, start_beat=10.0, position_x=-0.5))
        check("dirty again", doc.is_dirty(), "new edit re-sets the flag")
        doc.save()
        check("save clean", not doc.is_dirty(), "save() clears the flag")
        reopened = pk.Editor.open(tmp)
        check("reopen persists", reopened.chart().judge_line(0).note_count() == 3,
              "reopening the dir sees the saved flick note")

    print("\neditor_ops: all checks passed")


if __name__ == "__main__":
    main()
