"""Evaluate extra.json post-processing effects at arbitrary beats.

`ExtraRoot.parse` decodes the editor's extra.json (effects + BPM overrides);
`ExtraRoot.evaluate(beat)` resolves which effects are active at that beat and
returns `EvalEffect` objects (shader name, priority, global flag, and the
flat uniform values — keyframed vars interpolated, multi-value vars expanded
with `_0`, `_1`, ... suffixes). Effects come back sorted by priority.

Dependencies: only `phimakor`. No GUI, no chart directory.

Run:
    python example/extra_effects.py
"""

import json
import os
import sys

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))

import phimakor as pk

EXTRA_JSON = {
    "bpm": [{"time": [0, 2, 1], "bpm": 140.0}],
    "effects": [
        {
            "start": [0, 0, 1], "end": [0, 8, 1],       # beats 0..8
            "shader": "blur", "global": False, "priority": 0,
            "vars": {"radius": 2.0},
        },
        {
            "start": [0, 4, 1], "end": [0, 16, 1],     # beats 4..16
            "shader": "pixelate", "global": True, "priority": 5,
            "vars": {"intensity": 0.5, "vec2": [0.1, 0.2]},
        },
        {
            "start": [0, 12, 1], "end": [0, 20, 1],    # beats 12..20
            "shader": "water", "global": False, "priority": 2,
            "vars": {"amount": {
                "startTime": [0, 12, 1], "endTime": [0, 20, 1],
                "start": 0.0, "end": 1.0, "easingType": 1,
            }},
        },
    ],
}


def check(tag, cond, detail=""):
    assert cond, f"FAIL [{tag}] {detail}"
    print(f"  ok [{tag}] {detail}")


def summary(effects):
    # `global` is a Python keyword, so the getter is reached via getattr.
    return [(e.shader_name, e.priority, getattr(e, "global"), e.uniforms())
            for e in effects]


def main():
    print("== extra_effects: ExtraRoot.parse + evaluate ==")

    root = pk.ExtraRoot.parse(json.dumps(EXTRA_JSON))
    check("parse", len(root.evaluate(0.0)) >= 0, "extra.json accepted")

    # Beat 2: only blur is inside its [0, 8] window.
    at2 = root.evaluate(2.0)
    check("beat 2", [e.shader_name for e in at2] == ["blur"], str(summary(at2)))
    check("blur uniforms", at2[0].uniforms() == [2.0], str(at2[0].uniforms()))
    check("blur flags", not getattr(at2[0], "global") and at2[0].priority == 0,
          f"global={getattr(at2[0], 'global')}, priority={at2[0].priority}")

    # Beat 6: blur + pixelate, sorted by priority (0 before 5).
    at6 = root.evaluate(6.0)
    check("beat 6", [e.shader_name for e in at6] == ["blur", "pixelate"],
          str(summary(at6)))
    uni = at6[1].uniforms()
    check("pixelate uniforms",
          len(uni) == 3 and abs(uni[0] - 0.5) < 1e-6
          and abs(uni[1] - 0.1) < 1e-6 and abs(uni[2] - 0.2) < 1e-6
          and getattr(at6[1], "global") is True,
          f"uniforms={uni}, global={getattr(at6[1], 'global')}")

    # Beat 14: water (priority 2) before pixelate (5); the keyframed `amount`
    # var interpolates linearly to 0.25 at t=(14-12)/8.
    at14 = root.evaluate(14.0)
    check("beat 14", [e.shader_name for e in at14] == ["water", "pixelate"],
          str(summary(at14)))
    amount = at14[0].uniforms()[0]
    check("water keyframe", abs(amount - 0.25) < 1e-6, f"amount={amount:.4f}")

    # Beat 21: every effect window has ended.
    check("beat 21", root.evaluate(21.0) == [], "no active effects")

    # repr + bad input.
    check("repr", "EvalEffect(blur)" in repr(at2[0]), repr(at2[0]))
    try:
        pk.ExtraRoot.parse("{not json")
        check("bad json", False, "should have raised")
    except Exception as exc:  # PyRuntimeError
        check("bad json", "parse" in str(exc).lower(), type(exc).__name__)

    print("\nextra_effects: all checks passed")


if __name__ == "__main__":
    main()
