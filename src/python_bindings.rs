//! Python bindings for the Phimakor chart engine (`import phimakor`).
//!
//! Provides a Blender-like Python API for reading, editing, and evaluating
//! Phigros RPE charts. Accessible after building with `maturin build`.
//!
//! # Quick start
//!
//! ```text
//! import phimakor as pk
//!
//! # Open an editor session (with undo/redo)
//! doc = pk.Editor.open("path/to/chart")
//! print(f"Song: {doc.info().name} by {doc.info().composer}")
//!
//! # Read structure
//! for line in doc.chart().judge_lines():
//!     print(f"  Line {line.name}: {line.note_count()} notes")
//!
//! # Edit
//! note = pk.Note(kind=1, start_beat=4.0, position_x=0.3)
//! doc.add_note(line=0, note=note)
//! doc.undo()
//! doc.save()
//!
//! # Evaluate game state at a time
//! chart = pk.Chart.open("path/to/chart")
//! state = chart.state_at(time=10.5)
//! for ls in state.lines:
//!     print(f"  alpha={ls.alpha:.2f}, rotation={ls.rotation:.2f}")
//! ```

use crate::core::{
    bpm::{BpmList, Triple},
    chart::Chart,
    edit::{ChartDocument, EditError, EventKind},
    extra::{parse_extra, EvalEffect, ExtraRoot},
    model::{
        ChartInfo, RPEChart, RPEEvent, RPEJudgeLine, RPENote,
    },
};
use pyo3::prelude::*;

impl From<EditError> for PyErr {
    fn from(e: EditError) -> Self {
        pyo3::exceptions::PyRuntimeError::new_err(format!("{e}"))
    }
}

/// Chart metadata from `info.json` / `info.txt`.
///
/// Read-only metadata container: song name, difficulty, charter, composer, etc.
#[pyclass(unsendable, name = "ChartInfo")]
struct PyChartInfo(ChartInfo);

#[pymethods]
impl PyChartInfo {
    #[getter]
    fn name(&self) -> &str { &self.0.name }
    #[getter]
    fn difficulty(&self) -> f32 { self.0.difficulty }
    #[getter]
    fn level(&self) -> &str { &self.0.level }
    #[getter]
    fn charter(&self) -> &str { &self.0.charter }
    #[getter]
    fn composer(&self) -> &str { &self.0.composer }
    #[getter]
    fn music(&self) -> &str { &self.0.music }
    #[getter]
    fn illustration(&self) -> &str { &self.0.illustration }
    #[getter]
    fn offset(&self) -> f32 { self.0.offset }
    fn __repr__(&self) -> String { format!("ChartInfo({})", self.0.name) }
}

/// BPM timeline for beat <-> time conversion.
///
/// Convert between beat positions and wall-clock seconds using the chart's
/// BPM list. Call methods mutably — internal cursor caches the last lookup
/// for O(log n) amortized access.
#[pyclass(unsendable, name = "BpmList")]
struct PyBpmList(BpmList);

#[pymethods]
impl PyBpmList {
    fn time_beats(&mut self, beats: f64) -> f64 { self.0.time_beats(beats) }
    fn beat(&mut self, time: f64) -> f64 { self.0.beat(time) }
}

/// A keyframe event on a judge line (alpha, move, rotate, or speed).
///
/// Interpolates from `start` to `end` across `[start_beat, end_beat]`
/// using the easing function identified by `easing_type` (RPE easing ID 0–29).
///
/// ```python
/// ev = pk.Event(start=0.0, end=1.0, start_beat=0.0, end_beat=4.0, easing_type=1)
/// ```
#[pyclass(unsendable, name = "Event")]
#[derive(Clone)]
struct PyRPEEvent {
    inner: RPEEvent<f32>,
}

#[pymethods]
impl PyRPEEvent {
    #[new]
    #[pyo3(signature = (start=0.0, end=0.0, start_beat=0.0, end_beat=1.0, easing_type=1))]
    fn new(start: f32, end: f32, start_beat: f64, end_beat: f64, easing_type: i32) -> Self {
        Self {
            inner: RPEEvent {
                start,
                end,
                start_time: Triple::from_beats(start_beat),
                end_time: Triple::from_beats(end_beat),
                easing_type,
                easing_left: 0.0,
                easing_right: 1.0,
                bezier: 0,
                bezier_points: [0.0; 4],
            },
        }
    }

    #[getter]
    fn start(&self) -> f32 { self.inner.start }
    #[setter]
    fn set_start(&mut self, v: f32) { self.inner.start = v; }
    #[getter]
    fn end(&self) -> f32 { self.inner.end }
    #[setter]
    fn set_end(&mut self, v: f32) { self.inner.end = v; }
    #[getter]
    fn start_beat(&self) -> f64 { self.inner.start_time.beats() }
    #[setter]
    fn set_start_beat(&mut self, v: f64) { self.inner.start_time = Triple::from_beats(v); }
    #[getter]
    fn end_beat(&self) -> f64 { self.inner.end_time.beats() }
    #[setter]
    fn set_end_beat(&mut self, v: f64) { self.inner.end_time = Triple::from_beats(v); }
    #[getter]
    fn easing_type(&self) -> i32 { self.inner.easing_type }
    #[setter]
    fn set_easing_type(&mut self, v: i32) { self.inner.easing_type = v; }
    fn __repr__(&self) -> String {
        format!("Event({}->{} @ {:.2}-{:.2})", self.inner.start, self.inner.end,
                self.inner.start_time.beats(), self.inner.end_time.beats())
    }
}

/// A note on a judge line (tap / drag / hold / flick).
///
/// ```python
/// # Tap at beat 4.0, x=0.3
/// n = pk.Note(kind=1, start_beat=4.0, position_x=0.3, speed=1.0)
/// ```
#[pyclass(unsendable, name = "Note")]
#[derive(Clone)]
struct PyRPENote {
    inner: RPENote,
}

#[pymethods]
impl PyRPENote {
    #[new]
    #[pyo3(signature = (kind=1, start_beat=0.0, position_x=0.0, above=true, speed=1.0, size=1.0, is_fake=false))]
    fn new(kind: u8, start_beat: f64, position_x: f32, above: bool, speed: f32, size: f32, is_fake: bool) -> Self {
        Self {
            inner: RPENote {
                kind,
                start_time: Triple::from_beats(start_beat),
                position_x,
                above: if above { 1 } else { 0 },
                speed,
                size,
                is_fake: if is_fake { 1 } else { 0 },
                ..Default::default()
            },
        }
    }

    #[getter]
    fn kind(&self) -> u8 { self.inner.kind }
    #[setter]
    fn set_kind(&mut self, v: u8) { self.inner.kind = v; }
    #[getter]
    fn beat(&self) -> f64 { self.inner.start_time.beats() }
    #[setter]
    fn set_beat(&mut self, v: f64) { self.inner.start_time = Triple::from_beats(v); }
    #[getter]
    fn position_x(&self) -> f32 { self.inner.position_x }
    #[setter]
    fn set_position_x(&mut self, v: f32) { self.inner.position_x = v; }
    #[getter]
    fn y_offset(&self) -> f32 { self.inner.y_offset }
    #[setter]
    fn set_y_offset(&mut self, v: f32) { self.inner.y_offset = v; }
    #[getter]
    fn above(&self) -> bool { self.inner.above != 0 }
    #[setter]
    fn set_above(&mut self, v: bool) { self.inner.above = if v { 1 } else { 0 }; }
    #[getter]
    fn speed(&self) -> f32 { self.inner.speed }
    #[setter]
    fn set_speed(&mut self, v: f32) { self.inner.speed = v; }
    #[getter]
    fn size(&self) -> f32 { self.inner.size }
    #[setter]
    fn set_size(&mut self, v: f32) { self.inner.size = v; }
    #[getter]
    fn is_fake(&self) -> bool { self.inner.is_fake != 0 }
    #[setter]
    fn set_is_fake(&mut self, v: bool) { self.inner.is_fake = if v { 1 } else { 0 }; }
    #[getter]
    fn visible_time(&self) -> f64 { self.inner.visible_time }
    #[setter]
    fn set_visible_time(&mut self, v: f64) { self.inner.visible_time = v; }
    fn __repr__(&self) -> String {
        format!("Note(type={}, beat={:.2}, x={})", self.inner.kind, self.inner.start_time.beats(), self.inner.position_x)
    }
}

impl From<RPENote> for PyRPENote {
    fn from(inner: RPENote) -> Self { Self { inner } }
}

/// A judge line in the RPE chart model.
///
/// Contains notes and event layers. Access notes with `.notes()` and
/// events of a specific kind with `.events("alpha")`.
#[pyclass(unsendable, name = "JudgeLine")]
struct PyRPEJudgeLine {
    inner: RPEJudgeLine,
}

#[pymethods]
impl PyRPEJudgeLine {
    #[getter]
    fn name(&self) -> &str { &self.inner.name }
    #[getter]
    fn texture(&self) -> &str { &self.inner.texture }
    #[getter]
    fn z_order(&self) -> i32 { self.inner.z_order }

    fn notes(&self) -> Vec<PyRPENote> {
        self.inner.notes.as_ref().map(|ns| ns.iter().map(|n| PyRPENote { inner: n.clone() }).collect())
            .unwrap_or_default()
    }

    fn note_count(&self) -> usize {
        self.inner.notes.as_ref().map(|ns| ns.len()).unwrap_or(0)
    }

    #[pyo3(signature = (kind="alpha"))]
    fn events(&self, kind: &str) -> Vec<PyRPEEvent> {
        let layers = &self.inner.event_layers;
        let kind_idx = match kind {
            "alpha" => 0, "move_x" => 1, "move_y" => 2,
            "rotate" => 3, "speed" => 4, _ => return vec![],
        };
        let mut out = vec![];
        for layer in layers.iter().flatten() {
            let list = match kind_idx {
                0 => &layer.alpha_events, 1 => &layer.move_x_events,
                2 => &layer.move_y_events, 3 => &layer.rotate_events,
                4 => &layer.speed_events, _ => continue,
            };
            if let Some(evs) = list {
                out.extend(evs.iter().map(|e| PyRPEEvent { inner: e.clone() }));
            }
        }
        out
    }

    fn __repr__(&self) -> String { format!("JudgeLine({})", self.inner.name) }
}

/// The full RPE chart model (serde root).
///
/// Provides access to judge lines and their notes/events.
/// Read-only — use [`PyChartDocument`] (`.Editor`) for editing.
#[pyclass(unsendable, name = "RPEChart")]
struct PyRPEChart {
    inner: RPEChart,
}

#[pymethods]
impl PyRPEChart {
    fn judge_line_count(&self) -> usize { self.inner.judge_line_list.len() }

    fn judge_line(&self, idx: usize) -> PyResult<PyRPEJudgeLine> {
        self.inner.judge_line_list.get(idx)
            .map(|jl| PyRPEJudgeLine { inner: jl.clone() })
            .ok_or_else(|| EditError::BadOp(format!("line index {idx} out of range")).into())
    }

    fn judge_lines(&self) -> Vec<PyRPEJudgeLine> {
        self.inner.judge_line_list.iter()
            .map(|jl| PyRPEJudgeLine { inner: jl.clone() })
            .collect()
    }
}

/// One frame of evaluated chart state.
///
/// Produced by [`PyChart::state_at()`]. Contains the interpolated
/// transform and note visibility for every judge line at a specific
/// time in seconds.
#[pyclass(unsendable, name = "FrameState")]
struct PyFrameState {
    time: f64,
    line_states: Vec<PyLineState>,
}

#[pymethods]
impl PyFrameState {
    #[getter]
    fn time(&self) -> f64 { self.time }
    #[getter]
    fn lines(&self) -> Vec<PyLineState> { self.line_states.clone() }
}

/// Evaluated transform + notes for a single judge line at a moment in time.
#[pyclass(unsendable, name = "LineState")]
#[derive(Clone)]
struct PyLineState {
    alpha: f32,
    rotation: f32,
    position: [f32; 2],
    scale: [f32; 2],
    color: [f32; 4],
    texture: Option<String>,
    z_order: i32,
    note_states: Vec<PyNoteState>,
    parent: Option<usize>,
}

#[pymethods]
impl PyLineState {
    #[getter]
    fn alpha(&self) -> f32 { self.alpha }
    #[getter]
    fn rotation(&self) -> f32 { self.rotation }
    #[getter]
    fn position(&self) -> [f32; 2] { self.position }
    #[getter]
    fn scale(&self) -> [f32; 2] { self.scale }
    #[getter]
    fn color(&self) -> [f32; 4] { self.color }
    #[getter]
    fn texture(&self) -> Option<&str> { self.texture.as_deref() }
    #[getter]
    fn z_order(&self) -> i32 { self.z_order }
    #[getter]
    fn notes(&self) -> Vec<PyNoteState> { self.note_states.clone() }
    #[getter]
    fn parent(&self) -> Option<usize> { self.parent }
}

/// A visible note after evaluation (position relative to its judge line).
#[pyclass(unsendable, name = "NoteState")]
#[derive(Clone)]
struct PyNoteState {
    kind: u8,
    time: f64,
    relative_x: f32,
    relative_y: f32,
    alpha: f32,
    scale: f32,
    above: bool,
    fake: bool,
}

#[pymethods]
impl PyNoteState {
    #[getter]
    fn kind(&self) -> u8 { self.kind }
    #[getter]
    fn time(&self) -> f64 { self.time }
    #[getter]
    fn relative_x(&self) -> f32 { self.relative_x }
    #[getter]
    fn relative_y(&self) -> f32 { self.relative_y }
    #[getter]
    fn alpha(&self) -> f32 { self.alpha }
    #[getter]
    fn scale(&self) -> f32 { self.scale }
    #[getter]
    fn above(&self) -> bool { self.above }
    #[getter]
    fn fake(&self) -> bool { self.fake }
}

/// Low-level chart evaluation engine.
///
/// Loads a chart directory and evaluates note/line state at arbitrary
/// points in time. Useful for headless analysis and rendering.
///
/// ```text
/// chart = pk.Chart.open("path/to/chart")
/// state = chart.state_at(time=10.5)
/// ```
#[pyclass(unsendable, name = "Chart")]
struct PyChart {
    inner: Chart,
}

#[pymethods]
impl PyChart {
    #[staticmethod]
    fn open(dir: &str) -> PyResult<Self> {
        let (_info, chart) = Chart::load(std::path::Path::new(dir))
            .map_err(|e| EditError::BadOp(format!("{e}")))?;
        Ok(Self { inner: chart })
    }

    fn state_at(&mut self, time: f64) -> PyFrameState {
        let s = self.inner.state_at(time);
        PyFrameState {
            time: s.time,
            line_states: s.lines.iter().map(|l| PyLineState {
                alpha: l.alpha, rotation: l.rotation, position: l.position,
                scale: l.scale, color: l.color, texture: l.texture.clone(),
                z_order: l.z_order, parent: l.parent,
                note_states: l.notes.iter().map(|n| PyNoteState {
                    kind: n.kind, time: n.time, relative_x: n.relative[0], relative_y: n.relative[1],
                    alpha: n.alpha, scale: n.scale, above: n.above, fake: n.fake,
                }).collect(),
            }).collect(),
        }
    }

    fn duration(&self) -> f64 { self.inner.duration() }
    fn offset(&self) -> f32 { self.inner.offset() }
    fn note_count(&self) -> usize { self.inner.note_count() }
    fn line_count(&self) -> usize { self.inner.line_count() }
    fn time_to_beat(&mut self, time: f64) -> f64 { self.inner.time_to_beat(time) }
    fn line_name(&self, i: usize) -> Option<&str> {
        if i < self.inner.line_count() { Some(self.inner.line_name(i)) } else { None }
    }
}

/// Editor session with undo/redo and background autosave.
///
/// Open a chart directory, add/remove notes and events, undo/redo
/// operations, and save back to disk.
///
/// ```text
/// doc = pk.Editor.open("path/to/chart")
/// n = pk.Note(kind=1, start_beat=4.0, position_x=0.0)
/// doc.add_note(line=0, note=n)
/// doc.undo()
/// doc.save()
/// ```
#[pyclass(unsendable, name = "Editor")]
struct PyChartDocument {
    inner: ChartDocument,
}

#[pymethods]
impl PyChartDocument {
    #[staticmethod]
    fn open(dir: &str) -> PyResult<Self> {
        let doc = ChartDocument::open(std::path::Path::new(dir))
            .map_err(|e| PyErr::from(e))?;
        Ok(Self { inner: doc })
    }

    fn info(&self) -> PyChartInfo { PyChartInfo(self.inner.info().clone()) }
    fn chart(&self) -> PyRPEChart { PyRPEChart { inner: self.inner.chart().clone() } }

    fn add_note(&mut self, line: usize, note: &PyRPENote) -> PyResult<usize> {
        self.inner.add_note(line, note.inner.clone()).map_err(|e| PyErr::from(e))
    }

    fn remove_note(&mut self, line: usize, index: usize) -> PyResult<PyRPENote> {
        self.inner.remove_note(line, index).map(PyRPENote::from).map_err(|e| PyErr::from(e))
    }

    /// `kind` is one of: "alpha", "move_x", "move_y", "rotate", "speed".
    fn add_event(&mut self, line: usize, layer: usize, kind: &str, event: &PyRPEEvent) -> PyResult<()> {
        let ek = match kind {
            "alpha" => EventKind::Alpha, "move_x" => EventKind::MoveX,
            "move_y" => EventKind::MoveY, "rotate" => EventKind::Rotate,
            "speed" => EventKind::Speed, _ => return Err(EditError::BadOp(format!("unknown event kind {kind}")).into()),
        };
        self.inner.add_event(line, layer, ek, event.inner.clone()).map_err(|e| PyErr::from(e))
    }

    /// `kind` is one of: "alpha", "move_x", "move_y", "rotate", "speed".
    fn remove_event(&mut self, line: usize, layer: usize, kind: &str, index: usize) -> PyResult<PyRPEEvent> {
        let ek = match kind {
            "alpha" => EventKind::Alpha, "move_x" => EventKind::MoveX,
            "move_y" => EventKind::MoveY, "rotate" => EventKind::Rotate,
            "speed" => EventKind::Speed, _ => return Err(EditError::BadOp(format!("unknown event kind {kind}")).into()),
        };
        self.inner.remove_event(line, layer, ek, index)
            .map(|e| PyRPEEvent { inner: e }).map_err(|e| PyErr::from(e))
    }

    fn undo(&mut self) -> bool { self.inner.undo() }
    fn redo(&mut self) -> bool { self.inner.redo() }
    fn can_undo(&self) -> bool { self.inner.can_undo() }
    fn can_redo(&self) -> bool { self.inner.can_redo() }
    fn is_dirty(&self) -> bool { self.inner.is_dirty() }

    fn save(&mut self) -> PyResult<()> {
        self.inner.save().map_err(|e| PyErr::from(e))
    }
}

/// Parsed `extra.json` containing effect overrides and BPM adjustments.
#[pyclass(unsendable, name = "ExtraRoot")]
struct PyExtraRoot(ExtraRoot);

#[pymethods]
impl PyExtraRoot {
    #[staticmethod]
    fn parse(json: &str) -> PyResult<Self> {
        parse_extra(json.as_bytes())
            .map(Self)
            .map_err(|e| EditError::BadOp(format!("{e}")).into())
    }

    fn evaluate(&self, beat: f64) -> Vec<PyEvalEffect> {
        crate::core::extra::evaluate_effects(&self.0, beat)
            .into_iter().map(PyEvalEffect).collect()
    }
}

/// A single active effect resolved at a specific beat.
#[pyclass(unsendable, name = "EvalEffect")]
struct PyEvalEffect(EvalEffect);

#[pymethods]
impl PyEvalEffect {
    #[getter]
    fn shader_name(&self) -> &str { &self.0.shader_name }
    #[getter]
    fn priority(&self) -> u32 { self.0.priority }
    #[getter]
    fn global(&self) -> bool { self.0.global }
    fn uniforms(&self) -> Vec<f32> { self.0.uniforms.clone() }
    fn __repr__(&self) -> String { format!("EvalEffect({})", self.0.shader_name) }
}

/// Detect chart format by file content. Returns "rpe", "pec", "pgr", "pss", or "unknown".
#[pyfunction]
fn detect_format(path: &str) -> PyResult<String> {
    let bytes = std::fs::read(std::path::Path::new(path))
        .map_err(|e| EditError::BadOp(format!("{e}")))?;
    Ok(crate::core::chart_format::detect_format(&bytes).to_string())
}

/// Parse a chart file (raw RPE/PEC/PGR/PSS bytes) into an [`RPEChart`].
#[pyfunction]
fn parse_chart(format: &str, path: &str) -> PyResult<PyRPEChart> {
    let bytes = std::fs::read(std::path::Path::new(path))
        .map_err(|e| EditError::BadOp(format!("{e}")))?;
    let info = ChartInfo::default();
    let chart = crate::core::chart_format::parse_chart(format, &bytes, &info)
        .map_err(|e| EditError::BadOp(format!("{e}")))?;
    Ok(PyRPEChart { inner: chart })
}

/// Register all Python-exposed classes and functions.
pub fn register(m: &Bound<'_, pyo3::types::PyModule>) -> PyResult<()> {
    m.add_class::<PyChartInfo>()?;
    m.add_class::<PyBpmList>()?;
    m.add_class::<PyRPEEvent>()?;
    m.add_class::<PyRPENote>()?;
    m.add_class::<PyRPEJudgeLine>()?;
    m.add_class::<PyRPEChart>()?;
    m.add_class::<PyFrameState>()?;
    m.add_class::<PyLineState>()?;
    m.add_class::<PyNoteState>()?;
    m.add_class::<PyChart>()?;
    m.add_class::<PyChartDocument>()?;
    m.add_class::<PyExtraRoot>()?;
    m.add_class::<PyEvalEffect>()?;
    m.add_function(wrap_pyfunction!(detect_format, m)?)?;
    m.add_function(wrap_pyfunction!(parse_chart, m)?)?;
    Ok(())
}
