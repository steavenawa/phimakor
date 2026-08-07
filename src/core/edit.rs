#![allow(dead_code)] // 库 API: Python 绑定/embedding/备用接口,主程序未全部使用

//! Editor-facing document API: an [`RPEChart`] opened for mutation, with
//! undo/redo command history (操作分支), debounced background saving, and
//! first-class judge-line split/bind ops (拆线/绑线).
//!
//! History is the classic command pattern stored as *data* (the `Inverse`
//! enum — no closures): every applied op records everything needed to undo
//! AND redo it. The stacks are linear for now; the intended 操作分支 upgrade
//! path is a history tree — give each entry a parent link and per-node
//! children, and branch instead of clearing the redo stack on new edits.

use std::{
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        mpsc::{self, Receiver, Sender},
        Arc,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use super::{
    chart::load_info,
    model::{ChartInfo, InfoYaml, RPEBpmItem, RPEChart, RPEEvent, RPEEventLayer, RPEJudgeLine, RPENote},
};
use crate::core::bpm::Triple;

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Errors that can occur during chart editing operations.
#[derive(Debug)]
pub enum EditError {
    Io(std::io::Error),
    Json(serde_json::Error),
    BadOp(String),
}

impl std::fmt::Display for EditError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "io error: {e}"),
            Self::Json(e) => write!(f, "json error: {e}"),
            Self::BadOp(msg) => write!(f, "bad operation: {msg}"),
        }
    }
}

impl std::error::Error for EditError {}

impl From<std::io::Error> for EditError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

impl From<serde_json::Error> for EditError {
    fn from(e: serde_json::Error) -> Self {
        Self::Json(e)
    }
}

// ---------------------------------------------------------------------------
// Events
// ---------------------------------------------------------------------------

/// The five per-layer event lists (extended events are out of scope).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EventKind {
    Alpha,
    MoveX,
    MoveY,
    Rotate,
    Speed,
}

impl EventKind {
    const ALL: [EventKind; 5] = [
        Self::Alpha,
        Self::MoveX,
        Self::MoveY,
        Self::Rotate,
        Self::Speed,
    ];

    fn idx(self) -> usize {
        match self {
            Self::Alpha => 0,
            Self::MoveX => 1,
            Self::MoveY => 2,
            Self::Rotate => 3,
            Self::Speed => 4,
        }
    }
}

fn kind_events(layer: &mut RPEEventLayer, kind: EventKind) -> &mut Option<Vec<RPEEvent>> {
    match kind {
        EventKind::Alpha => &mut layer.alpha_events,
        EventKind::MoveX => &mut layer.move_x_events,
        EventKind::MoveY => &mut layer.move_y_events,
        EventKind::Rotate => &mut layer.rotate_events,
        EventKind::Speed => &mut layer.speed_events,
    }
}

fn kind_events_ref(layer: &RPEEventLayer, kind: EventKind) -> &Option<Vec<RPEEvent>> {
    match kind {
        EventKind::Alpha => &layer.alpha_events,
        EventKind::MoveX => &layer.move_x_events,
        EventKind::MoveY => &layer.move_y_events,
        EventKind::Rotate => &layer.rotate_events,
        EventKind::Speed => &layer.speed_events,
    }
}

fn empty_layer() -> RPEEventLayer {
    RPEEventLayer {
        alpha_events: None,
        move_x_events: None,
        move_y_events: None,
        rotate_events: None,
        speed_events: None,
    }
}

// ---------------------------------------------------------------------------
// History (command pattern, inverse ops as data)
// ---------------------------------------------------------------------------

/// One applied edit, carrying everything needed to undo and redo it.
/// Variant names name the op that was performed; the payload is what the
/// inverse needs (plus what redo needs to re-apply deterministically).
enum Inverse {
    /// Op: note inserted at (line, index). Undo removes it; redo re-inserts.
    AddNote {
        line: usize,
        index: usize,
        note: RPENote,
    },
    /// Op: note removed from (line, index). Undo re-inserts; redo removes.
    RemoveNote {
        line: usize,
        index: usize,
        note: RPENote,
    },
    /// Op: note at (line, index) replaced. Undo restores `old`; redo `new`.
    ReplaceNote {
        line: usize,
        index: usize,
        old: RPENote,
        new: RPENote,
    },
    /// Op: a batch of notes on `line` replaced as ONE step (multi-select
    /// drag / nudge, PMCORE-18). Undo restores all `old`; redo re-applies
    /// all `new`. Each entry keeps its slot — no re-sort.
    ReplaceNotesMulti {
        line: usize,
        old: Vec<(usize, RPENote)>,
        new: Vec<(usize, RPENote)>,
    },
    /// Op: a batch of notes inserted into `line` as ONE step (复制粘贴 /
    /// 整组放置,PMCORE-XX)。`items` 按插入后的最终索引升序。Undo 按
    /// 索引降序全部移除;redo 每颗按存储索引重新插入。
    AddNotesMulti {
        line: usize,
        items: Vec<(usize, RPENote)>,
    },
    AddEvent {
        line: usize,
        layer: usize,
        kind: EventKind,
        index: usize,
        ev: RPEEvent,
    },
    RemoveEvent {
        line: usize,
        layer: usize,
        kind: EventKind,
        index: usize,
        ev: RPEEvent,
    },
    /// Op: event at (line, layer, kind, index) replaced. Undo restores
    /// `old`; redo restores `new`.
    ReplaceEvent {
        line: usize,
        layer: usize,
        kind: EventKind,
        index: usize,
        old: RPEEvent,
        new: RPEEvent,
    },
    /// Op: a batch of events in the (line, layer, kind) list replaced as
    /// ONE step (事件框选批量平移, PMCORE-20). Undo restores all `old`;
    /// redo re-applies all `new`. Each entry keeps its slot — no re-sort.
    ReplaceEventsMulti {
        line: usize,
        layer: usize,
        kind: EventKind,
        old: Vec<(usize, RPEEvent)>,
        new: Vec<(usize, RPEEvent)>,
    },
    /// Op: a batch of events removed from the (line, layer, kind) list as
    /// ONE step (事件框选批量删除, PMCORE-20). `items` sorted ascending by
    /// index. Undo re-inserts each at its stored index; redo re-removes in
    /// descending index order.
    RemoveEventsMulti {
        line: usize,
        layer: usize,
        kind: EventKind,
        items: Vec<(usize, RPEEvent)>,
    },
    /// Op: `line` split at `at_beats`, producing `new_index`. Undo merges
    /// `new_index` back into `line`; redo re-splits.
    SplitLine {
        line: usize,
        new_index: usize,
        at_beats: f64,
    },
    /// Op: `source` merged into `target` and removed. Undo re-inserts
    /// `removed_line` at `source`, truncates target's notes/event lists back
    /// to the recorded lengths, and restores remapped father indices.
    BindLines {
        target: usize,
        source: usize,
        removed_line: Box<RPEJudgeLine>,
        target_notes_len: usize,
        /// Per pre-merge target event-layer entry: `None` = the entry itself
        /// was `None`; inner `None` per kind = that event list was `None`.
        target_layer_lens: Vec<Option<[Option<usize>; 5]>>,
        /// (line index, old parent) in pre-removal index space.
        father_remap: Vec<(usize, Option<isize>)>,
    },
    /// Op: BPM entry inserted at `index`. Undo removes it; redo re-inserts.
    AddBpm {
        index: usize,
        item: RPEBpmItem,
    },
    /// Op: BPM entry removed from `index`. Undo re-inserts; redo removes.
    RemoveBpm {
        index: usize,
        item: RPEBpmItem,
    },
    /// Op: BPM entry at `index` replaced. Undo restores `old`; redo `new`.
    ReplaceBpm {
        index: usize,
        old: RPEBpmItem,
        new: RPEBpmItem,
    },
    /// Op: empty judge line inserted at `index`. Undo removes it (fathers
    /// beyond shift down); redo re-inserts (fathers beyond shift up).
    AddLine {
        index: usize,
        line: RPEJudgeLine,
    },
    /// Op: judge line removed from `index`. Undo re-inserts and restores the
    /// recorded father indices; redo re-removes with the same remap.
    RemoveLine {
        index: usize,
        line: RPEJudgeLine,
        father_remap: Vec<(usize, Option<isize>)>,
    },
}

// ---------------------------------------------------------------------------
// Background saver
// ---------------------------------------------------------------------------

enum SaveMsg {
    /// Latest chart state to write (supersedes any earlier snapshot).
    Snapshot(Box<RPEChart>),
    /// Write the pending snapshot now and report the result.
    Flush(Sender<Result<(), EditError>>),
    /// Write any pending snapshot, then exit the thread.
    Shutdown,
}

/// Writes `chart` as JSON to `dir/name` via a tmp file + rename. Before
/// replacing the main file, the previous version is kept as `dir/<name>.bak`
/// (PMCORE-24 崩溃恢复:手动损坏主文件或上次写一半时可回滚)。
fn write_atomic(dir: &Path, name: &str, chart: &RPEChart) -> Result<(), EditError> {
    let json = serde_json::to_string(chart)?;
    write_bytes_atomic(dir, name, json.as_bytes())
}

/// tmp+rename 原子写任意字节内容;旧版本保留为 `dir/<name>.bak`(与
/// [`write_atomic`] 同模式,PMCORE-24)。info 文件写回复用(PMCORE-23)。
fn write_bytes_atomic(dir: &Path, name: &str, bytes: &[u8]) -> Result<(), EditError> {
    let tmp = dir.join(format!("{name}.tmp"));
    std::fs::write(&tmp, bytes)?;
    let dst = dir.join(name);
    // PMCORE-24:.bak 保留上一次成功写入的版本(尽力而为,失败不阻塞主
    // 保存)。copy 不保留 mtime,因此成功保存后主文件必然比 .bak 新——
    // 启动检测正是靠这个对比判断上次是否正常退出。
    let bak = dir.join(format!("{name}.bak"));
    if dst.exists() {
        let _ = std::fs::copy(&dst, &bak);
    }
    // ponytail: std rename won't overwrite an existing file on Windows, so
    // remove first. Crash in the remove→rename window loses the old file.
    if let Err(e) = std::fs::remove_file(&dst) {
        if e.kind() != std::io::ErrorKind::NotFound {
            return Err(EditError::Io(e));
        }
    }
    std::fs::rename(&tmp, &dst)?;
    Ok(())
}

/// Debounced writer: holds only the newest snapshot and writes it within
/// ~`interval_ms` of the last write. Exits on `Shutdown` or channel
/// disconnect.
fn saver_loop(rx: Receiver<SaveMsg>, dir: PathBuf, name: String, interval_ms: Arc<AtomicU64>) {
    let mut pending: Option<Box<RPEChart>> = None;
    let mut last_write = Instant::now();
    loop {
        // PMCORE-24:防抖间隔读设置(默认 1s,设置面板实时改)。
        let debounce = Duration::from_millis(interval_ms.load(Ordering::Relaxed));
        let timeout = if pending.is_some() {
            debounce.saturating_sub(last_write.elapsed())
        } else {
            Duration::from_secs(3600)
        };
        match rx.recv_timeout(timeout) {
            Ok(SaveMsg::Snapshot(chart)) => {
                // PMCORE-24:首个 pending 快照到达时重置防抖窗口,保证 burst
                // 连击合并为一次写,避免"写中间快照"导致 .bak 内容不确定。
                if pending.is_none() {
                    last_write = Instant::now();
                }
                pending = Some(chart);
            }
            Ok(SaveMsg::Flush(reply)) => {
                let result = match pending.take() {
                    Some(chart) => write_atomic(&dir, &name, &chart),
                    None => Ok(()),
                };
                last_write = Instant::now();
                let _ = reply.send(result);
            }
            Ok(SaveMsg::Shutdown) => {
                if let Some(chart) = pending.take() {
                    let _ = write_atomic(&dir, &name, &chart);
                }
                break;
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if let Some(chart) = pending.take() {
                    // ponytail: a failed periodic write is dropped silently;
                    // the doc stays dirty, so the next flush reports it.
                    if write_atomic(&dir, &name, &chart).is_ok() {
                        last_write = Instant::now();
                    }
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
}

// ---------------------------------------------------------------------------
// Small merge/split helpers
// ---------------------------------------------------------------------------

fn merge_notes(dst: &mut Option<Vec<RPENote>>, src: Option<Vec<RPENote>>) {
    let Some(src) = src else { return };
    match dst {
        Some(d) => {
            d.extend(src);
            d.sort_by(|a, b| a.start_time.beats().total_cmp(&b.start_time.beats()));
        }
        None => *dst = Some(src),
    }
}

fn merge_events(dst: &mut Option<Vec<RPEEvent>>, src: Option<Vec<RPEEvent>>) {
    let Some(src) = src else { return };
    match dst {
        Some(d) => {
            d.extend(src);
            d.sort_by(|a, b| a.start_time.beats().total_cmp(&b.start_time.beats()));
        }
        None => *dst = Some(src),
    }
}

/// Splits one event list at `at_beats`: events starting at/after go right,
/// events ending at/before stay left, and crossing events are duplicated —
/// clamped to `[start, at]` on the left and `[at, end]` on the right with
/// the boundary value linearly interpolated.
fn split_event_list(
    list: Option<Vec<RPEEvent>>,
    at_beats: f64,
    at: Triple,
) -> (Option<Vec<RPEEvent>>, Option<Vec<RPEEvent>>) {
    let Some(events) = list else {
        return (None, None);
    };
    let mut left = Vec::new();
    let mut right = Vec::new();
    for ev in events {
        let s = ev.start_time.beats();
        let e = ev.end_time.beats();
        if s >= at_beats {
            right.push(ev);
        } else if e <= at_beats {
            left.push(ev);
        } else {
            let t = ((at_beats - s) / (e - s)) as f32;
            let mid = ev.start + (ev.end - ev.start) * t;
            let mut l = ev.clone();
            l.end = mid;
            l.end_time = at;
            let mut r = ev;
            r.start = mid;
            r.start_time = at;
            left.push(l);
            right.push(r);
        }
    }
    (Some(left), Some(right))
}

/// After inserting a line at `at`, every father index >= `at` shifts up one;
/// negative fathers (RPE's -1 = no father) stay.
fn remap_parents_insert(lines: &mut [RPEJudgeLine], at: usize) {
    for l in lines {
        if let Some(p) = l.parent {
            if p >= at as isize {
                l.parent = Some(p + 1);
            }
        }
    }
}

/// Before removing the line at `removed`: fathers pointing at it become -1
/// (deliberate simplification — they lose their parent rather than
/// inheriting the removed line's father; documented on
/// [`ChartDocument::bind_lines`]), fathers beyond shift down one. Returns
/// (line index, old parent) pairs in pre-removal index space.
fn remap_parents_remove(lines: &mut [RPEJudgeLine], removed: usize) -> Vec<(usize, Option<isize>)> {
    let mut changed = Vec::new();
    for (i, l) in lines.iter_mut().enumerate() {
        if i == removed {
            continue;
        }
        let new = match l.parent {
            Some(p) if p == removed as isize => Some(-1),
            Some(p) if p > removed as isize => Some(p - 1),
            other => other,
        };
        if new != l.parent {
            changed.push((i, l.parent));
            l.parent = new;
        }
    }
    changed
}

// ---------------------------------------------------------------------------
// Info metadata (PMCORE-23)
// ---------------------------------------------------------------------------

/// info 文件的源格式:写回时按同格式序列化。txt(legacy)源升级为
/// info.json(与 `load_info` 的 json → yml → txt 回退优先级一致)。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum InfoSource {
    Json,
    Yaml,
    Txt,
}

impl InfoSource {
    /// 按文件存在性探测源格式(与 `load_info` 的回退顺序一致)。
    fn detect(dir: &Path) -> Self {
        if dir.join("info.json").is_file() {
            InfoSource::Json
        } else if dir.join("info.yml").is_file() {
            InfoSource::Yaml
        } else {
            InfoSource::Txt
        }
    }
}

/// Chart 面板可编辑的元数据字段(PMCORE-23)。chart/music/illustration 等
/// 路径字段不在其中——它们由加载/导入流程维护,禁止面板修改。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InfoField {
    Name,
    Composer,
    Charter,
    Illustrator,
    Level,
    Difficulty,
}

// ---------------------------------------------------------------------------
// ChartDocument
// ---------------------------------------------------------------------------

/// An RPE chart directory opened for editing: metadata + chart in memory,
/// undo/redo history, and a debounced background save thread.
pub struct ChartDocument {
    dir: PathBuf,
    info: ChartInfo,
    chart: RPEChart,
    dirty: bool,
    undo_stack: Vec<Inverse>,
    redo_stack: Vec<Inverse>,
    /// 源谱面是否为 PEC 文本格式(open 时按 detect 结果记录):PEC 不允许覆写保存。
    source_is_pec: bool,
    /// PMCORE-23:info 源格式(写回目标)。
    info_source: InfoSource,
    /// PMCORE-23:有未写回的元数据修改(仅 save/flush 消费,autosave 不写 info)。
    info_dirty: bool,
    saver: Option<Sender<SaveMsg>>,
    saver_handle: Option<JoinHandle<()>>,
    /// PMCORE-24:自动保存开关(编辑后调 save_background)。默认关——由
    /// 宿主按 SettingsData.autosave 打开,保持库/测试默认行为不变。
    autosave: bool,
    /// saver 线程的防抖间隔(毫秒),设置面板实时改(Arc 共享)。
    saver_interval_ms: Arc<AtomicU64>,
}

impl ChartDocument {
    /// Opens the chart directory (`info.json`, or an RPE-export `info.txt`,
    /// plus the chart file it names) and spawns the background save thread.
    pub fn open(dir: &Path) -> Result<Self, EditError> {
        let info = load_info(dir).map_err(|e| EditError::BadOp(format!("{e:#}")))?;
        let info_source = InfoSource::detect(dir);
        let chart_path = dir.join(&info.chart);
        let bytes = std::fs::read(&chart_path).map_err(|e| EditError::BadOp(format!("{e:#}")))?;
        let fmt = super::chart_format::detect_format(&bytes);
        let source_is_pec = fmt == "pec";
        let chart = super::chart_format::parse_chart(fmt, &bytes, &info).map_err(|e| EditError::BadOp(format!("{e:#}")))?;
        let (tx, rx) = mpsc::channel::<SaveMsg>();
        let saver_dir = dir.to_path_buf();
        let chart_name = info.chart.clone();
        let interval_ms = Arc::new(AtomicU64::new(1000));
        let saver_interval_ms = Arc::clone(&interval_ms);
        let saver_handle = thread::spawn(move || saver_loop(rx, saver_dir, chart_name, interval_ms));
        Ok(Self {
            dir: dir.to_path_buf(),
            info,
            chart,
            dirty: false,
            source_is_pec,
            info_source,
            info_dirty: false,
            undo_stack: Vec::new(),
            redo_stack: Vec::new(),
            saver: Some(tx),
            saver_handle: Some(saver_handle),
            autosave: false,
            saver_interval_ms,
        })
    }

    /// PMCORE-24:设置自动保存开关与防抖间隔(毫秒)。由设置面板在文档
    /// 加载/设置变更时调用;默认关(与 `open` 一致),宿主显式开启。
    pub fn set_autosave(&mut self, on: bool, interval_ms: u64) {
        self.autosave = on;
        self.saver_interval_ms.store(interval_ms, Ordering::Relaxed);
    }

    /// Returns a reference to the parsed chart metadata.
    pub fn info(&self) -> &ChartInfo {
        &self.info
    }

    /// Modifies one editable metadata field (PMCORE-23). Not recorded in the
    /// undo stack (metadata is out of history, same decision as the Eff
    /// panel); only sets the info-dirty flag. `Difficulty` parses `value` as
    /// f32 and clamps to ≥ 0; `Level` is a free string.
    pub fn set_info_field(&mut self, field: InfoField, value: String) -> Result<(), EditError> {
        match field {
            InfoField::Name => self.info.name = value,
            InfoField::Composer => self.info.composer = value,
            InfoField::Charter => self.info.charter = value,
            InfoField::Illustrator => self.info.illustrator = value,
            InfoField::Level => self.info.level = value,
            InfoField::Difficulty => {
                let v: f32 = value
                    .trim()
                    .parse()
                    .map_err(|_| EditError::BadOp(format!("difficulty 需要是数字,得到 {value:?}")))?;
                self.info.difficulty = v.max(0.0);
            }
        }
        self.info_dirty = true;
        Ok(())
    }

    /// True when there are unwritten info changes (consumed by `save`/`flush`).
    pub fn info_dirty(&self) -> bool {
        self.info_dirty
    }

    /// Writes the metadata back to the info file in its source format
    /// (PMCORE-23): `info.json` for JSON sources, `info.yml` for YAML
    /// sources, and a fresh `info.json` for legacy `info.txt` sources
    /// (upgrade). Atomic tmp+rename; on failure `info_dirty` stays set so the
    /// next save/flush retries.
    pub fn save_info(&mut self) -> Result<(), EditError> {
        let (name, bytes) = match self.info_source {
            InfoSource::Json | InfoSource::Txt => {
                ("info.json", serde_json::to_vec_pretty(&self.info)?)
            }
            InfoSource::Yaml => {
                let yaml = serde_yaml::to_string(&InfoYaml::from(&self.info))
                    .map_err(|e| EditError::BadOp(format!("序列化 info.yml: {e}")))?;
                ("info.yml", yaml.into_bytes())
            }
        };
        write_bytes_atomic(&self.dir, name, &bytes)?;
        self.info_dirty = false;
        Ok(())
    }

    /// Returns a reference to the in-memory chart model.
    pub fn chart(&self) -> &RPEChart {
        &self.chart
    }

    /// (beat, bpm) 行——BPM 面板表单构建与提交 diff 共用同一提取。
    pub fn bpm_pairs(&self) -> Vec<(f64, f64)> {
        self.chart
            .bpm_list
            .iter()
            .map(|b| (b.start_time.beats(), b.bpm))
            .collect()
    }

    /// True when edits exist that neither `save` nor `save_background` has
    /// acknowledged. Cleared by both; set again by any later edit.
    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    // --- note ops ---

    /// Inserts `note` into `line`, keeping the list sorted by startTime
    /// (binary search; equal times insert after existing ones). Returns the
    /// inserted index.
    pub fn add_note(&mut self, line: usize, note: RPENote) -> Result<usize, EditError> {
        let notes = self.line_mut(line)?.notes.get_or_insert_with(Vec::new);
        let beats = note.start_time.beats();
        let index = notes.partition_point(|n| n.start_time.beats() <= beats);
        notes.insert(index, note.clone());
        self.record(Inverse::AddNote { line, index, note });
        Ok(index)
    }

    /// Inserts a batch of notes into `line` as ONE undo op — 复制粘贴 /
    /// 整组放置(PMCORE-XX)。每颗按 startTime 稳定排序插入(同拍时现有
    /// 音符在前、新音符在后,与 `add_note` 的 partition_point 语义一致),
    /// 记录各颗插入后的最终索引。原子:仅 line 越界会失败,失败前不写
    /// 入任何变更。空批次不写入、不入栈(避免空 undo 步)。
    pub fn add_notes_multi(&mut self, line: usize, notes: &[RPENote]) -> Result<(), EditError> {
        if notes.is_empty() {
            return Ok(());
        }
        let items = {
            let line_notes = self.line_mut(line)?;
            let dst = line_notes.notes.get_or_insert_with(Vec::new);
            let mut merged: Vec<(bool, RPENote)> = dst
                .iter()
                .cloned()
                .map(|n| (false, n))
                .chain(notes.iter().cloned().map(|n| (true, n)))
                .collect();
            merged.sort_by(|a, b| a.1.start_time.beats().total_cmp(&b.1.start_time.beats()));
            let items: Vec<(usize, RPENote)> = merged
                .iter()
                .enumerate()
                .filter(|(_, (is_new, _))| *is_new)
                .map(|(i, (_, n))| (i, n.clone()))
                .collect();
            *dst = merged.into_iter().map(|(_, n)| n).collect();
            items
        };
        self.record(Inverse::AddNotesMulti { line, items });
        Ok(())
    }

    /// Removes the note at `index` in the given `line`, returning the removed note.
    pub fn remove_note(&mut self, line: usize, index: usize) -> Result<RPENote, EditError> {
        let notes = self.notes_mut(line)?;
        if index >= notes.len() {
            return Err(EditError::BadOp(format!(
                "note index {index} out of range in line {line} ({} notes)",
                notes.len()
            )));
        }
        let note = notes.remove(index);
        self.record(Inverse::RemoveNote {
            line,
            index,
            note: note.clone(),
        });
        Ok(note)
    }

    /// Replaces the note at (line, index), returning the old one. Does not
    /// re-sort: the new note keeps the slot.
    pub fn replace_note(
        &mut self,
        line: usize,
        index: usize,
        note: RPENote,
    ) -> Result<RPENote, EditError> {
        let notes = self.notes_mut(line)?;
        if index >= notes.len() {
            return Err(EditError::BadOp(format!(
                "note index {index} out of range in line {line} ({} notes)",
                notes.len()
            )));
        }
        let old = std::mem::replace(&mut notes[index], note.clone());
        self.record(Inverse::ReplaceNote {
            line,
            index,
            old: old.clone(),
            new: note,
        });
        Ok(old)
    }

    /// Replaces a batch of notes in `line` (each `(index, note)` keeps its
    /// slot, no re-sort) as ONE undo op — multi-select drag / nudge / batch
    /// delete 的一次性提交接口(PMCORE-18)。原子:任一索引越界则整体报错,
    /// 不写入任何变更、不入栈。
    pub fn replace_notes_multi(
        &mut self,
        line: usize,
        changes: &[(usize, RPENote)],
    ) -> Result<(), EditError> {
        let notes = self.notes_mut(line)?;
        for (i, _) in changes {
            if *i >= notes.len() {
                return Err(EditError::BadOp(format!(
                    "note index {i} out of range in line {line} ({} notes)",
                    notes.len()
                )));
            }
        }
        let old: Vec<(usize, RPENote)> = changes
            .iter()
            .map(|(i, _)| (*i, notes[*i].clone()))
            .collect();
        for (i, n) in changes {
            notes[*i] = n.clone();
        }
        self.record(Inverse::ReplaceNotesMulti {
            line,
            old,
            new: changes.to_vec(),
        });
        Ok(())
    }

    // --- comment ops (PMCORE-77) ---

    /// Sets or clears the comment on note `(line, index)`; `None` clears.
    /// Comments ride on the note itself ([`RPENote::comment`]), so they move
    /// with the note when it is dragged and are lost when the note is
    /// deleted. Not recorded in the undo stack (metadata-like, same decision
    /// as the info fields); only marks dirty so the save pipeline
    /// (Ctrl+S / autosave) picks it up.
    pub fn set_note_comment(
        &mut self,
        line: usize,
        index: usize,
        comment: Option<String>,
    ) -> Result<(), EditError> {
        let notes = self.notes_mut(line)?;
        let len = notes.len();
        let note = notes.get_mut(index).ok_or_else(|| {
            EditError::BadOp(format!(
                "note index {index} out of range in line {line} ({len} notes)"
            ))
        })?;
        note.comment = comment;
        self.touch_dirty();
        Ok(())
    }

    /// Returns the comment on note `(line, index)`, if any.
    pub fn note_comment(&self, line: usize, index: usize) -> Option<&str> {
        self.chart
            .judge_line_list
            .get(line)?
            .notes
            .as_ref()?
            .get(index)?
            .comment
            .as_deref()
    }

    /// Sets or clears the comment on judge line `line`. Same semantics as
    /// [`set_note_comment`](Self::set_note_comment).
    pub fn set_line_comment(
        &mut self,
        line: usize,
        comment: Option<String>,
    ) -> Result<(), EditError> {
        self.line_mut(line)?.comment = comment;
        self.touch_dirty();
        Ok(())
    }

    /// Returns the comment on judge line `line`, if any.
    pub fn line_comment(&self, line: usize) -> Option<&str> {
        self.chart.judge_line_list.get(line)?.comment.as_deref()
    }

    // --- event ops ---

    /// Inserts `ev` into the (line, layer, kind) event list, sorted by
    /// startTime. Missing layers/lists are created on demand. Returns the
    /// inserted index (mirrors `add_note`).
    pub fn add_event(
        &mut self,
        line: usize,
        layer: usize,
        kind: EventKind,
        ev: RPEEvent<f32>,
    ) -> Result<usize, EditError> {
        let list = self.event_list_mut(line, layer, kind)?;
        let beats = ev.start_time.beats();
        let index = list.partition_point(|e| e.start_time.beats() <= beats);
        list.insert(index, ev.clone());
        self.record(Inverse::AddEvent {
            line,
            layer,
            kind,
            index,
            ev,
        });
        Ok(index)
    }

    /// Removes the event at `index` from the specified (line, layer, kind)
    /// event list, returning the removed event.
    pub fn remove_event(
        &mut self,
        line: usize,
        layer: usize,
        kind: EventKind,
        index: usize,
    ) -> Result<RPEEvent<f32>, EditError> {
        let list = self.events_mut_strict(line, layer, kind)?;
        if index >= list.len() {
            return Err(EditError::BadOp(format!(
                "{kind:?} event index {index} out of range in line {line} layer {layer} ({} events)",
                list.len()
            )));
        }
        let ev = list.remove(index);
        self.record(Inverse::RemoveEvent {
            line,
            layer,
            kind,
            index,
            ev: ev.clone(),
        });
        Ok(ev)
    }

    /// Replaces the event at `index` in the specified (line, layer, kind)
    /// event list with `ev`, returning the old event. A single undoable op
    /// (unlike remove+add, which pollutes the undo stack with two entries).
    pub fn replace_event(
        &mut self,
        line: usize,
        layer: usize,
        kind: EventKind,
        index: usize,
        ev: RPEEvent<f32>,
    ) -> Result<RPEEvent<f32>, EditError> {
        let list = self.events_mut_strict(line, layer, kind)?;
        if index >= list.len() {
            return Err(EditError::BadOp(format!(
                "{kind:?} event index {index} out of range in line {line} layer {layer} ({} events)",
                list.len()
            )));
        }
        let old = std::mem::replace(&mut list[index], ev.clone());
        self.record(Inverse::ReplaceEvent {
            line,
            layer,
            kind,
            index,
            old: old.clone(),
            new: ev,
        });
        Ok(old)
    }

    /// Replaces a batch of events in the (line, layer, kind) list (each
    /// `(index, event)` keeps its slot, no re-sort) as ONE undo op — 事件
    /// 框选批量平移(PMCORE-20)。原子:任一索引越界则整体报错,不写入
    /// 任何变更、不入栈。
    pub fn replace_events_multi(
        &mut self,
        line: usize,
        layer: usize,
        kind: EventKind,
        changes: &[(usize, RPEEvent)],
    ) -> Result<(), EditError> {
        let list = self.events_mut_strict(line, layer, kind)?;
        for (i, _) in changes {
            if *i >= list.len() {
                return Err(EditError::BadOp(format!(
                    "{kind:?} event index {i} out of range in line {line} layer {layer} ({} events)",
                    list.len()
                )));
            }
        }
        let old: Vec<(usize, RPEEvent)> = changes
            .iter()
            .map(|(i, _)| (*i, list[*i].clone()))
            .collect();
        for (i, e) in changes {
            list[*i] = e.clone();
        }
        self.record(Inverse::ReplaceEventsMulti {
            line,
            layer,
            kind,
            old,
            new: changes.to_vec(),
        });
        Ok(())
    }

    /// Removes a batch of events from the (line, layer, kind) list as ONE
    /// undo op — 事件框选批量删除(PMCORE-20)。`indices` 去重后按降序移除
    /// (索引不漂移);任一索引越界则整体报错,不写入任何变更、不入栈。
    pub fn remove_events_multi(
        &mut self,
        line: usize,
        layer: usize,
        kind: EventKind,
        indices: &[usize],
    ) -> Result<(), EditError> {
        let list = self.events_mut_strict(line, layer, kind)?;
        let mut idxs: Vec<usize> = indices.to_vec();
        idxs.sort_unstable();
        idxs.dedup();
        for i in &idxs {
            if *i >= list.len() {
                return Err(EditError::BadOp(format!(
                    "{kind:?} event index {i} out of range in line {line} layer {layer} ({} events)",
                    list.len()
                )));
            }
        }
        let items: Vec<(usize, RPEEvent)> = idxs.iter().map(|i| (*i, list[*i].clone())).collect();
        for i in idxs.iter().rev() {
            list.remove(*i);
        }
        self.record(Inverse::RemoveEventsMulti {
            line,
            layer,
            kind,
            items,
        });
        Ok(())
    }

    // --- line structure ops (拆/绑线) ---

    /// 拆线: splits `line` at `at_beats`. Notes with startTime < `at_beats`
    /// stay, the rest move to the new line. Every event list (alpha/moveX/
    /// moveY/rotate/speed) splits the same way; events crossing the point
    /// are duplicated and clamped on both sides, with the boundary value
    /// linearly interpolated. The new line is inserted right after the
    /// original (its index is returned), named `"<name> (split)"`, copying
    /// texture/father/zOrder/isCover — lines fathered to the original keep
    /// pointing at the original, and father indices beyond the insertion
    /// point shift up one. Extended and ctrl events stay on the original
    /// line (out of scope for now).
    ///
    /// Undo merges the new line back; crossing events then remain as two
    /// clamped halves (value-equivalent for linear easings, not structurally
    /// identical to the original event).
    pub fn split_line(&mut self, line: usize, at_beats: f64) -> Result<usize, EditError> {
        let new_index = self.split_raw(line, at_beats)?;
        self.record(Inverse::SplitLine {
            line,
            new_index,
            at_beats,
        });
        Ok(new_index)
    }

    /// 绑线: merges `source`'s notes (time-sorted) and event layers (per
    /// kind, time-sorted; target layers are extended if `source` has more)
    /// into `target`, then removes `source`. Times are preserved. Father
    /// indices are remapped: fathers pointing beyond `source` shift down
    /// one; fathers pointing AT `source` are set to -1 (deliberate
    /// simplification — they lose their parent instead of inheriting
    /// `source`'s father). `source`'s extended/ctrl events do not merge;
    /// they live on in the undo record only.
    ///
    /// Undo restores counts, not exact membership, if source/target event
    /// or note times interleave (merge sorts; undo truncates to recorded
    /// lengths).
    pub fn bind_lines(&mut self, target: usize, source: usize) -> Result<(), EditError> {
        let inverse = self.bind_raw(target, source)?;
        self.record(inverse);
        Ok(())
    }

    /// 加线: appends a new empty judge line (no notes/events) at the end of
    /// the list, with z-order one above the current top. Returns its index.
    pub fn add_line(&mut self, name: String, texture: String) -> Result<usize, EditError> {
        let max_z = self.chart.judge_line_list.iter().map(|l| l.z_order).max().unwrap_or(0);
        let line = RPEJudgeLine {
            name,
            texture,
            parent: None,
            rotate_with_father: None,
            event_layers: vec![None],
            extended: None,
            notes: None,
            is_cover: 0,
            z_order: max_z + 1,
            attach_ui: None,
            pos_control: vec![],
            size_control: vec![],
            alpha_control: vec![],
            y_control: vec![],
            comment: None,
        };
        let index = self.chart.judge_line_list.len();
        self.chart.judge_line_list.push(line.clone());
        self.record(Inverse::AddLine { index, line });
        Ok(index)
    }

    /// 删线: removes the judge line at `index`, returning it. Father links
    /// are remapped: lines fathered to it become fatherless (-1), lines
    /// beyond it shift down one.
    pub fn remove_line(&mut self, index: usize) -> Result<RPEJudgeLine, EditError> {
        let lines = &mut self.chart.judge_line_list;
        if lines.len() <= 1 {
            return Err(EditError::BadOp("cannot remove the last judge line".into()));
        }
        if index >= lines.len() {
            return Err(EditError::BadOp(format!(
                "line index {index} out of range ({} lines)",
                lines.len()
            )));
        }
        let line = lines[index].clone();
        let father_remap = remap_parents_remove(lines, index);
        lines.remove(index);
        self.record(Inverse::RemoveLine {
            index,
            line: line.clone(),
            father_remap,
        });
        Ok(line)
    }

    // --- bpm ops ---

    /// Inserts a BPM change point at `beat`, keeping the list sorted by
    /// start time. Returns the inserted index.
    pub fn add_bpm(&mut self, bpm: f64, beat: f64) -> Result<usize, EditError> {
        let list = &mut self.chart.bpm_list;
        let index = list.partition_point(|b| b.start_time.beats() <= beat);
        let item = RPEBpmItem {
            bpm,
            start_time: Triple::from_beats(beat),
        };
        list.insert(index, item.clone());
        self.record(Inverse::AddBpm { index, item });
        Ok(index)
    }

    /// Removes the BPM entry at `index`, returning it. Refuses to remove
    /// the last remaining entry (charts must keep at least one).
    pub fn remove_bpm(&mut self, index: usize) -> Result<RPEBpmItem, EditError> {
        let list = &mut self.chart.bpm_list;
        if list.len() <= 1 {
            return Err(EditError::BadOp("cannot remove the last BPM entry".into()));
        }
        if index >= list.len() {
            return Err(EditError::BadOp(format!(
                "bpm index {index} out of range ({} entries)",
                list.len()
            )));
        }
        let item = list.remove(index);
        self.record(Inverse::RemoveBpm {
            index,
            item: item.clone(),
        });
        Ok(item)
    }

    /// Replaces the BPM entry at `index` with a new value/beat, returning
    /// the old one. Does not re-sort: the new entry keeps the slot.
    pub fn replace_bpm(&mut self, index: usize, bpm: f64, beat: f64) -> Result<RPEBpmItem, EditError> {
        let old = self.chart.bpm_list.get(index).cloned().ok_or_else(|| {
            EditError::BadOp(format!(
                "bpm index {index} out of range ({} entries)",
                self.chart.bpm_list.len()
            ))
        })?;
        let new = RPEBpmItem {
            bpm,
            start_time: Triple::from_beats(beat),
        };
        self.chart.bpm_list[index] = new.clone();
        self.record(Inverse::ReplaceBpm {
            index,
            old: old.clone(),
            new,
        });
        Ok(old)
    }

    // --- history ---

    /// Reverts the most recent edit. Returns `true` if an edit was undone,
    /// `false` if the undo stack was empty.
    pub fn undo(&mut self) -> bool {
        let Some(inv) = self.undo_stack.pop() else {
            return false;
        };
        match self.apply_inverse(&inv) {
            Ok(()) => {
                self.redo_stack.push(inv);
                self.dirty = true;
                self.autosave_touch();
                true
            }
            Err(_) => {
                self.undo_stack.push(inv);
                false
            }
        }
    }

    /// Re-applies the most recently undone edit. Returns `true` if an edit
    /// was redone, `false` if the redo stack was empty.
    pub fn redo(&mut self) -> bool {
        let Some(inv) = self.redo_stack.pop() else {
            return false;
        };
        match self.apply_forward(&inv) {
            Ok(()) => {
                self.undo_stack.push(inv);
                self.dirty = true;
                self.autosave_touch();
                true
            }
            Err(_) => {
                self.redo_stack.push(inv);
                false
            }
        }
    }

    /// Returns `true` when there are edits that can be undone.
    pub fn can_undo(&self) -> bool {
        !self.undo_stack.is_empty()
    }

    /// Returns `true` when there are undone edits that can be redone.
    pub fn can_redo(&self) -> bool {
        !self.redo_stack.is_empty()
    }

    // --- save ---

    /// Writes the current chart synchronously (tmp file + rename).
    pub fn save(&mut self) -> Result<(), EditError> {
        // PMCORE-23:info 有变更时先写回(失败保持 info_dirty,下次重试)。
        // info 写回与 chart 格式无关,故放在 PEC 守卫之前——PEC 源谱面的
        // 元数据编辑同样可持久化。干净会话不碰 info 文件(无副作用)。
        if self.info_dirty {
            self.save_info()?;
        }
        // PEC 源谱面拒绝覆写保存:write_atomic 只写 JSON,会把 .pec 覆写成
        // 不可解析的 JSON(bug 76ef60f9)。提示另存为 RPE JSON。
        if self.source_is_pec {
            return Err(EditError::BadOp("PEC 谱面不支持覆写保存,请另存为 RPE JSON".into()));
        }
        write_atomic(&self.dir, &self.info.chart, &self.chart)?;
        self.dirty = false;
        Ok(())
    }

    /// Marks save intent: hands a snapshot of the current chart to the
    /// background thread, which writes it within ~1s. Safe to call after
    /// every edit. Write failures surface at the next [`flush`](Self::flush)
    /// (and leave `is_dirty` set).
    pub fn save_background(&mut self) {
        // PEC 源同上:后台快照同样会以 JSON 覆写 .pec,直接跳过(dirty 保持,提示由 save() 的 Err 承担)。
        if self.source_is_pec {
            return;
        }
        if let Some(tx) = &self.saver {
            if tx
                .send(SaveMsg::Snapshot(Box::new(self.chart.clone())))
                .is_ok()
            {
                self.dirty = false;
            } else {
                self.saver = None; // thread is dead; flush() will report it
            }
        }
    }

    /// Joins the pending background save: blocks until the last snapshot
    /// handed to [`save_background`](Self::save_background) is written.
    /// Does not save edits made since — call [`save`](Self::save) (or
    /// `save_background` again) for those. Call before preview reload/exit.
    pub fn flush(&mut self) -> Result<(), EditError> {
        let Some(tx) = &self.saver else {
            return if self.dirty {
                Err(EditError::BadOp("background save thread is dead".into()))
            } else {
                Ok(())
            };
        };
        let (reply_tx, reply_rx) = mpsc::channel();
        tx.send(SaveMsg::Flush(reply_tx))
            .map_err(|_| EditError::BadOp("background save thread is dead".into()))?;
        let chart_result = reply_rx
            .recv()
            .map_err(|_| EditError::BadOp("background save thread is dead".into()))?;
        // PMCORE-23:显式提交(退出/切谱/关窗)时把未写回的 info 变更落盘;
        // 失败保持 info_dirty(save() 会重试)。
        if self.info_dirty {
            self.save_info()?;
        }
        chart_result
    }

    // --- internals ---

    fn record(&mut self, inv: Inverse) {
        self.undo_stack.push(inv);
        self.redo_stack.clear();
        self.dirty = true;
        self.autosave_touch();
    }

    /// Marks dirty + triggers autosave without touching the undo stack
    /// (comment/info edits, PMCORE-77/23).
    fn touch_dirty(&mut self) {
        self.dirty = true;
        self.autosave_touch();
    }

    /// PMCORE-24:编辑入栈(或 undo/redo)后触发自动保存——快照交给后台
    /// saver 线程防抖落盘。PEC 源由 save_background 自行拒绝。
    fn autosave_touch(&mut self) {
        if self.autosave {
            self.save_background();
        }
    }

    fn line_mut(&mut self, line: usize) -> Result<&mut RPEJudgeLine, EditError> {
        let len = self.chart.judge_line_list.len();
        self.chart.judge_line_list.get_mut(line).ok_or_else(|| {
            EditError::BadOp(format!("line index {line} out of range ({len} lines)"))
        })
    }

    fn notes_mut(&mut self, line: usize) -> Result<&mut Vec<RPENote>, EditError> {
        self.line_mut(line)?
            .notes
            .as_mut()
            .ok_or_else(|| EditError::BadOp(format!("line {line} has no notes")))
    }

    /// Creating accessor: missing layers/lists spring into existence.
    fn event_list_mut(
        &mut self,
        line: usize,
        layer: usize,
        kind: EventKind,
    ) -> Result<&mut Vec<RPEEvent>, EditError> {
        let l = self.line_mut(line)?;
        if l.event_layers.len() <= layer {
            l.event_layers.resize(layer + 1, None);
        }
        let layer_ref = l.event_layers[layer].get_or_insert_with(empty_layer);
        Ok(kind_events(layer_ref, kind).get_or_insert_with(Vec::new))
    }

    fn events_mut_strict(
        &mut self,
        line: usize,
        layer: usize,
        kind: EventKind,
    ) -> Result<&mut Vec<RPEEvent>, EditError> {
        let l = self.line_mut(line)?;
        let layer_ref = l
            .event_layers
            .get_mut(layer)
            .and_then(Option::as_mut)
            .ok_or_else(|| EditError::BadOp(format!("line {line} layer {layer} is empty")))?;
        kind_events(layer_ref, kind).as_mut().ok_or_else(|| {
            EditError::BadOp(format!("line {line} layer {layer} has no {kind:?} events"))
        })
    }

    fn split_raw(&mut self, line: usize, at_beats: f64) -> Result<usize, EditError> {
        let lines = &mut self.chart.judge_line_list;
        let len = lines.len();
        let orig = lines.get_mut(line).ok_or_else(|| {
            EditError::BadOp(format!("line index {line} out of range ({len} lines)"))
        })?;
        let at = Triple::from_beats(at_beats);

        let new_notes = match orig.notes.take() {
            Some(notes) => {
                let (before, after): (Vec<_>, Vec<_>) = notes
                    .into_iter()
                    .partition(|n| n.start_time.beats() < at_beats);
                orig.notes = Some(before);
                Some(after)
            }
            None => None,
        };

        let mut new_layers: Vec<Option<RPEEventLayer>> =
            Vec::with_capacity(orig.event_layers.len());
        for layer in orig.event_layers.iter_mut() {
            match layer.as_mut() {
                Some(l) => {
                    let mut nl = empty_layer();
                    for kind in EventKind::ALL {
                        let (b, a) = split_event_list(kind_events(l, kind).take(), at_beats, at);
                        *kind_events(l, kind) = b;
                        *kind_events(&mut nl, kind) = a;
                    }
                    new_layers.push(Some(nl));
                }
                None => new_layers.push(None),
            }
        }

        let new_line = RPEJudgeLine {
            name: format!("{} (split)", orig.name),
            texture: orig.texture.clone(),
            parent: orig.parent,
            rotate_with_father: orig.rotate_with_father,
            event_layers: new_layers,
            extended: None, // extended events stay on the original line
            notes: new_notes,
            is_cover: orig.is_cover,
            z_order: orig.z_order,
            attach_ui: orig.attach_ui.clone(),
            pos_control: Vec::new(), // ctrl events stay on the original line
            size_control: Vec::new(),
            alpha_control: Vec::new(),
            y_control: Vec::new(),
            comment: orig.comment.clone(),
        };
        let new_index = line + 1;
        lines.insert(new_index, new_line);
        remap_parents_insert(lines, new_index);
        Ok(new_index)
    }

    fn bind_raw(&mut self, target: usize, source: usize) -> Result<Inverse, EditError> {
        let lines = &mut self.chart.judge_line_list;
        if target == source || target >= lines.len() || source >= lines.len() {
            return Err(EditError::BadOp(format!(
                "bind_lines: invalid target {target} / source {source} ({} lines)",
                lines.len()
            )));
        }
        let removed_line = Box::new(lines[source].clone());
        let target_notes_len = lines[target].notes.as_ref().map_or(0, Vec::len);
        let target_layer_lens: Vec<Option<[Option<usize>; 5]>> = lines[target]
            .event_layers
            .iter()
            .map(|entry| {
                entry.as_ref().map(|layer| {
                    let mut lens = [None; 5];
                    for kind in EventKind::ALL {
                        lens[kind.idx()] = kind_events_ref(layer, kind).as_ref().map(Vec::len);
                    }
                    lens
                })
            })
            .collect();

        {
            let (t_ref, s_ref) = if target < source {
                let (a, b) = lines.split_at_mut(source);
                (&mut a[target], &mut b[0])
            } else {
                let (a, b) = lines.split_at_mut(target);
                (&mut b[0], &mut a[source])
            };
            merge_notes(&mut t_ref.notes, s_ref.notes.take());
            if t_ref.event_layers.len() < s_ref.event_layers.len() {
                t_ref.event_layers.resize(s_ref.event_layers.len(), None);
            }
            for (tl, sl) in t_ref
                .event_layers
                .iter_mut()
                .zip(s_ref.event_layers.iter_mut())
            {
                match (tl, sl) {
                    (Some(t), Some(s)) => {
                        for kind in EventKind::ALL {
                            merge_events(kind_events(t, kind), kind_events(s, kind).take());
                        }
                    }
                    (slot @ None, Some(s)) => *slot = Some(std::mem::replace(s, empty_layer())),
                    _ => {}
                }
            }
        }

        let father_remap = remap_parents_remove(lines, source);
        lines.remove(source);
        Ok(Inverse::BindLines {
            target,
            source,
            removed_line,
            target_notes_len,
            target_layer_lens,
            father_remap,
        })
    }

    /// Undo of split: merge `new_index`'s notes/event layers back into
    /// `line` and remove it (crossing events stay as two clamped halves).
    fn merge_lines_back(&mut self, line: usize, new_index: usize) -> Result<(), EditError> {
        let lines = &mut self.chart.judge_line_list;
        if line >= new_index || new_index >= lines.len() {
            return Err(EditError::BadOp(format!(
                "cannot merge line {new_index} back into {line} ({} lines)",
                lines.len()
            )));
        }
        {
            let (a, b) = lines.split_at_mut(new_index);
            let dst = &mut a[line];
            let src = &mut b[0];
            merge_notes(&mut dst.notes, src.notes.take());
            for (dl, sl) in dst.event_layers.iter_mut().zip(src.event_layers.iter_mut()) {
                if let (Some(d), Some(s)) = (dl, sl) {
                    for kind in EventKind::ALL {
                        merge_events(kind_events(d, kind), kind_events(s, kind).take());
                    }
                }
            }
        }
        remap_parents_remove(lines, new_index);
        lines.remove(new_index);
        Ok(())
    }

    fn apply_inverse(&mut self, inv: &Inverse) -> Result<(), EditError> {
        match inv {
            Inverse::AddNote { line, index, .. } => {
                let notes = self.notes_mut(*line)?;
                if notes.is_empty() {
                    return Err(EditError::BadOp(format!(
                        "undo: line {line} has no notes to remove"
                    )));
                }
                notes.remove((*index).min(notes.len() - 1));
            }
            Inverse::RemoveNote { line, index, note } => {
                let notes = self.line_mut(*line)?.notes.get_or_insert_with(Vec::new);
                notes.insert((*index).min(notes.len()), note.clone());
            }
            Inverse::ReplaceNote {
                line, index, old, ..
            } => {
                let notes = self.notes_mut(*line)?;
                if let Some(slot) = notes.get_mut(*index) {
                    *slot = old.clone();
                }
            }
            Inverse::ReplaceNotesMulti { line, old, .. } => {
                let notes = self.notes_mut(*line)?;
                for (i, n) in old {
                    if let Some(slot) = notes.get_mut(*i) {
                        *slot = n.clone();
                    }
                }
            }
            Inverse::AddNotesMulti { line, items } => {
                let notes = self.notes_mut(*line)?;
                for (i, _) in items.iter().rev() {
                    if *i < notes.len() {
                        notes.remove(*i);
                    }
                }
            }
            Inverse::AddEvent {
                line,
                layer,
                kind,
                index,
                ..
            } => {
                let list = self.events_mut_strict(*line, *layer, *kind)?;
                if list.is_empty() {
                    return Err(EditError::BadOp(format!(
                        "undo: no {kind:?} events to remove in line {line} layer {layer}"
                    )));
                }
                list.remove((*index).min(list.len() - 1));
            }
            Inverse::RemoveEvent {
                line,
                layer,
                kind,
                index,
                ev,
            } => {
                let list = self.event_list_mut(*line, *layer, *kind)?;
                list.insert((*index).min(list.len()), ev.clone());
            }
            Inverse::ReplaceEvent {
                line,
                layer,
                kind,
                index,
                old,
                ..
            } => {
                let list = self.events_mut_strict(*line, *layer, *kind)?;
                if let Some(slot) = list.get_mut(*index) {
                    *slot = old.clone();
                }
            }
            Inverse::ReplaceEventsMulti {
                line,
                layer,
                kind,
                old,
                ..
            } => {
                let list = self.events_mut_strict(*line, *layer, *kind)?;
                for (i, e) in old {
                    if let Some(slot) = list.get_mut(*i) {
                        *slot = e.clone();
                    }
                }
            }
            Inverse::RemoveEventsMulti {
                line,
                layer,
                kind,
                items,
            } => {
                let list = self.event_list_mut(*line, *layer, *kind)?;
                for (i, e) in items {
                    list.insert((*i).min(list.len()), e.clone());
                }
            }
            Inverse::SplitLine {
                line, new_index, ..
            } => self.merge_lines_back(*line, *new_index)?,
            Inverse::BindLines {
                target,
                source,
                removed_line,
                target_notes_len,
                target_layer_lens,
                father_remap,
            } => {
                let lines = &mut self.chart.judge_line_list;
                lines.insert(*source, (**removed_line).clone());
                let t = &mut lines[*target];
                if let Some(notes) = t.notes.as_mut() {
                    notes.truncate(*target_notes_len);
                }
                for (entry, lens) in t.event_layers.iter_mut().zip(target_layer_lens.iter()) {
                    match (entry, lens) {
                        (Some(layer), Some(lens)) => {
                            for kind in EventKind::ALL {
                                let slot = kind_events(layer, kind);
                                match lens[kind.idx()] {
                                    Some(len) => {
                                        if let Some(list) = slot.as_mut() {
                                            list.truncate(len);
                                        }
                                    }
                                    None => *slot = None,
                                }
                            }
                        }
                        (entry @ Some(_), None) => *entry = None,
                        _ => {}
                    }
                }
                t.event_layers.truncate(target_layer_lens.len());
                for (i, old) in father_remap {
                    if let Some(l) = lines.get_mut(*i) {
                        l.parent = *old;
                    }
                }
            }
            Inverse::AddBpm { index, .. } => {
                let list = &mut self.chart.bpm_list;
                if list.is_empty() {
                    return Err(EditError::BadOp("undo: no BPM entries to remove".into()));
                }
                list.remove((*index).min(list.len() - 1));
            }
            Inverse::RemoveBpm { index, item } => {
                let list = &mut self.chart.bpm_list;
                list.insert((*index).min(list.len()), item.clone());
            }
            Inverse::ReplaceBpm { index, old, .. } => {
                if let Some(slot) = self.chart.bpm_list.get_mut(*index) {
                    *slot = old.clone();
                }
            }
            Inverse::AddLine { index, .. } => {
                let lines = &mut self.chart.judge_line_list;
                if lines.is_empty() {
                    return Err(EditError::BadOp("undo: no judge lines to remove".into()));
                }
                let index = (*index).min(lines.len() - 1);
                remap_parents_remove(lines, index);
                lines.remove(index);
            }
            Inverse::RemoveLine {
                index,
                line,
                father_remap,
            } => {
                let lines = &mut self.chart.judge_line_list;
                lines.insert((*index).min(lines.len()), line.clone());
                for (i, old) in father_remap {
                    if let Some(l) = lines.get_mut(*i) {
                        l.parent = *old;
                    }
                }
            }
        }
        Ok(())
    }

    fn apply_forward(&mut self, inv: &Inverse) -> Result<(), EditError> {
        match inv {
            Inverse::AddNote { line, index, note } => {
                let notes = self.line_mut(*line)?.notes.get_or_insert_with(Vec::new);
                notes.insert((*index).min(notes.len()), note.clone());
            }
            Inverse::RemoveNote { line, index, .. } => {
                let notes = self.notes_mut(*line)?;
                if notes.is_empty() {
                    return Err(EditError::BadOp(format!(
                        "redo: line {line} has no notes to remove"
                    )));
                }
                notes.remove((*index).min(notes.len() - 1));
            }
            Inverse::ReplaceNote {
                line, index, new, ..
            } => {
                let notes = self.notes_mut(*line)?;
                if let Some(slot) = notes.get_mut(*index) {
                    *slot = new.clone();
                }
            }
            Inverse::ReplaceNotesMulti {
                line, new, ..
            } => {
                let notes = self.notes_mut(*line)?;
                for (i, n) in new {
                    if let Some(slot) = notes.get_mut(*i) {
                        *slot = n.clone();
                    }
                }
            }
            Inverse::AddNotesMulti { line, items } => {
                let notes = self.line_mut(*line)?.notes.get_or_insert_with(Vec::new);
                for (i, n) in items {
                    notes.insert((*i).min(notes.len()), n.clone());
                }
            }
            Inverse::AddEvent {
                line,
                layer,
                kind,
                index,
                ev,
            } => {
                let list = self.event_list_mut(*line, *layer, *kind)?;
                list.insert((*index).min(list.len()), ev.clone());
            }
            Inverse::RemoveEvent {
                line,
                layer,
                kind,
                index,
                ..
            } => {
                let list = self.events_mut_strict(*line, *layer, *kind)?;
                if list.is_empty() {
                    return Err(EditError::BadOp(format!(
                        "redo: no {kind:?} events to remove in line {line} layer {layer}"
                    )));
                }
                list.remove((*index).min(list.len() - 1));
            }
            Inverse::ReplaceEvent {
                line,
                layer,
                kind,
                index,
                new,
                ..
            } => {
                let list = self.events_mut_strict(*line, *layer, *kind)?;
                if let Some(slot) = list.get_mut(*index) {
                    *slot = new.clone();
                }
            }
            Inverse::ReplaceEventsMulti {
                line,
                layer,
                kind,
                new,
                ..
            } => {
                let list = self.events_mut_strict(*line, *layer, *kind)?;
                for (i, e) in new {
                    if let Some(slot) = list.get_mut(*i) {
                        *slot = e.clone();
                    }
                }
            }
            Inverse::RemoveEventsMulti {
                line,
                layer,
                kind,
                items,
            } => {
                let list = self.events_mut_strict(*line, *layer, *kind)?;
                if list.is_empty() {
                    return Err(EditError::BadOp(format!(
                        "redo: no {kind:?} events to remove in line {line} layer {layer}"
                    )));
                }
                for (i, _) in items.iter().rev() {
                    list.remove((*i).min(list.len() - 1));
                }
            }
            Inverse::SplitLine { line, at_beats, .. } => {
                self.split_raw(*line, *at_beats)?;
            }
            Inverse::BindLines { target, source, .. } => {
                self.bind_raw(*target, *source)?;
            }
            Inverse::AddBpm { index, item } => {
                let list = &mut self.chart.bpm_list;
                list.insert((*index).min(list.len()), item.clone());
            }
            Inverse::RemoveBpm { index, .. } => {
                let list = &mut self.chart.bpm_list;
                if list.is_empty() {
                    return Err(EditError::BadOp("redo: no BPM entries to remove".into()));
                }
                list.remove((*index).min(list.len() - 1));
            }
            Inverse::ReplaceBpm { index, new, .. } => {
                if let Some(slot) = self.chart.bpm_list.get_mut(*index) {
                    *slot = new.clone();
                }
            }
            Inverse::AddLine { index, line } => {
                let lines = &mut self.chart.judge_line_list;
                let index = (*index).min(lines.len());
                lines.insert(index, line.clone());
                remap_parents_insert(lines, index);
            }
            Inverse::RemoveLine { index, .. } => {
                let lines = &mut self.chart.judge_line_list;
                if lines.is_empty() {
                    return Err(EditError::BadOp("redo: no judge lines to remove".into()));
                }
                let index = (*index).min(lines.len() - 1);
                remap_parents_remove(lines, index);
                lines.remove(index);
            }
        }
        Ok(())
    }
}

impl Drop for ChartDocument {
    /// Backstop: tells the saver thread to write any pending snapshot and
    /// joins it, so a forgotten `flush()` doesn't lose acknowledged edits.
    fn drop(&mut self) {
        if let Some(tx) = self.saver.take() {
            let _ = tx.send(SaveMsg::Shutdown);
        }
        if let Some(h) = self.saver_handle.take() {
            let _ = h.join();
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::model::{RPEBpmItem, RPEMetadata};
    use std::fs;
    use std::sync::atomic::{AtomicU32, Ordering};

    fn t(beats: f64) -> Triple {
        Triple::from_beats(beats)
    }

    fn note(beats: f64) -> RPENote {
        RPENote {
            kind: 1,
            above: 1,
            start_time: t(beats),
            end_time: t(beats),
            position_x: 0.,
            y_offset: 0.,
            alpha: 255,
            hitsound: None,
            size: 1.,
            speed: 1.,
            is_fake: 0,
            visible_time: 999999.,
            tint: None,
            tint_hit_effects: None,
            judge_area: None,
            comment: None,
        }
    }

    fn ev(s: f64, e: f64, start: f32, end: f32) -> RPEEvent {
        RPEEvent {
            easing_left: 0.,
            easing_right: 1.,
            bezier: 0,
            bezier_points: [0.; 4],
            easing_type: 1,
            start,
            end,
            start_time: t(s),
            end_time: t(e),
        }
    }

    fn layer_with(alpha: Vec<RPEEvent>, speed: Vec<RPEEvent>) -> RPEEventLayer {
        RPEEventLayer {
            alpha_events: Some(alpha),
            move_x_events: None,
            move_y_events: None,
            rotate_events: None,
            speed_events: Some(speed),
        }
    }

    fn line(
        name: &str,
        parent: Option<isize>,
        notes: Vec<RPENote>,
        layers: Vec<Option<RPEEventLayer>>,
    ) -> RPEJudgeLine {
        RPEJudgeLine {
            name: name.into(),
            texture: "line.png".into(),
            parent,
            rotate_with_father: None,
            event_layers: layers,
            extended: None,
            notes: Some(notes),
            is_cover: 1,
            z_order: 0,
            attach_ui: None,
            pos_control: vec![],
            size_control: vec![],
            alpha_control: vec![],
            y_control: vec![],
            comment: None,
        }
    }

    /// Three lines: A (notes 1..=4, alpha [0,1] and crossing [2,4]), B
    /// (notes 5,6, speed [0,8]), C (fathered to B, empty).
    fn fixture_chart() -> RPEChart {
        RPEChart {
            meta: RPEMetadata {
                offset: 0,
                rpe_version: 160,
            },
            bpm_list: vec![RPEBpmItem {
                bpm: 120.,
                start_time: t(0.),
            }],
            judge_line_list: vec![
                line(
                    "A",
                    None,
                    vec![note(1.), note(2.), note(3.), note(4.)],
                    vec![Some(layer_with(
                        vec![ev(0., 1., 0., 100.), ev(2., 4., 100., 200.)],
                        vec![],
                    ))],
                ),
                line(
                    "B",
                    None,
                    vec![note(5.), note(6.)],
                    vec![Some(layer_with(vec![], vec![ev(0., 8., 1., 1.)]))],
                ),
                line("C", Some(1), vec![], vec![]),
            ],
        }
    }

    static COUNTER: AtomicU32 = AtomicU32::new(0);

    fn temp_chart_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "phimakor-edit-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("info.json"),
            r#"{"chart": "chart.json", "name": "fixture"}"#,
        )
        .unwrap();
        fs::write(
            dir.join("chart.json"),
            serde_json::to_string(&fixture_chart()).unwrap(),
        )
        .unwrap();
        dir
    }

    fn note_beats(doc: &ChartDocument, line: usize) -> Vec<f64> {
        doc.chart().judge_line_list[line]
            .notes
            .as_ref()
            .unwrap()
            .iter()
            .map(|n| n.start_time.beats())
            .collect()
    }

    #[test]
    fn add_remove_undo_redo_roundtrip() {
        let dir = temp_chart_dir();
        let mut doc = ChartDocument::open(&dir).unwrap();
        assert!(!doc.is_dirty() && !doc.can_undo() && !doc.can_redo());

        // add: time-sorted insertion between beats 2 and 3
        let idx = doc.add_note(0, note(2.5)).unwrap();
        assert_eq!(idx, 2);
        assert_eq!(note_beats(&doc, 0), vec![1., 2., 2.5, 3., 4.]);
        assert!(doc.is_dirty() && doc.can_undo());

        // remove: returns the removed note
        let removed = doc.remove_note(0, idx).unwrap();
        assert_eq!(removed.start_time.beats(), 2.5);
        assert_eq!(note_beats(&doc, 0), vec![1., 2., 3., 4.]);

        // undo removal -> note returns at the same slot
        assert!(doc.undo());
        assert_eq!(note_beats(&doc, 0), vec![1., 2., 2.5, 3., 4.]);

        // redo -> removed again
        assert!(doc.redo());
        assert_eq!(note_beats(&doc, 0), vec![1., 2., 3., 4.]);

        // undo both ops -> back to fixture; redo both -> roundtrip complete
        assert!(doc.undo()); // un-remove
        assert!(doc.undo()); // un-add
        assert_eq!(note_beats(&doc, 0), vec![1., 2., 3., 4.]);
        assert!(!doc.can_undo());
        assert!(doc.redo()); // re-add
        assert!(doc.redo()); // re-remove
        assert_eq!(note_beats(&doc, 0), vec![1., 2., 3., 4.]);

        // replace roundtrip
        let old = doc.replace_note(0, 0, note(1.5)).unwrap();
        assert_eq!(old.start_time.beats(), 1.);
        assert_eq!(note_beats(&doc, 0)[0], 1.5);
        assert!(doc.undo());
        assert_eq!(note_beats(&doc, 0)[0], 1.);
        assert!(doc.redo());
        assert_eq!(note_beats(&doc, 0)[0], 1.5);

        // event add/remove roundtrip
        doc.add_event(0, 0, EventKind::MoveX, ev(1., 2., 0., 50.))
            .unwrap();
        doc.add_event(0, 0, EventKind::MoveX, ev(0., 1., 10., 20.))
            .unwrap();
        {
            let list = doc.chart().judge_line_list[0].event_layers[0]
                .as_ref()
                .unwrap()
                .move_x_events
                .as_ref()
                .unwrap();
            assert_eq!(list.len(), 2);
            assert_eq!(list[0].start_time.beats(), 0.); // sorted
        }
        let removed_ev = doc.remove_event(0, 0, EventKind::MoveX, 0).unwrap();
        assert_eq!(removed_ev.start, 10.);
        assert!(doc.undo()); // un-remove
        assert!(doc.undo()); // un-add second
        assert!(doc.undo()); // un-add first
        assert_eq!(
            doc.chart().judge_line_list[0].event_layers[0]
                .as_ref()
                .unwrap()
                .move_x_events
                .as_ref()
                .unwrap()
                .len(),
            0
        );

        // replace_event: 单 op 替换 + undo 恢复 old / redo 恢复 new。
        doc.add_event(0, 0, EventKind::MoveX, ev(0., 1., 10., 20.))
            .unwrap();
        let old_ev = doc.replace_event(0, 0, EventKind::MoveX, 0, ev(0., 1., 99., 88.)).unwrap();
        assert_eq!(old_ev.start, 10.);
        assert_eq!(
            doc.chart().judge_line_list[0].event_layers[0]
                .as_ref().unwrap().move_x_events.as_ref().unwrap()[0].start,
            99.
        );
        assert!(doc.undo()); // 一步撤销整个替换
        assert_eq!(
            doc.chart().judge_line_list[0].event_layers[0]
                .as_ref().unwrap().move_x_events.as_ref().unwrap()[0].start,
            10.
        );
        assert!(doc.redo()); // 一步重做
        assert_eq!(
            doc.chart().judge_line_list[0].event_layers[0]
                .as_ref().unwrap().move_x_events.as_ref().unwrap()[0].start,
            99.
        );

        drop(doc);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn replace_notes_multi_single_undo_op() {
        // PMCORE-18:批量替换(多选拖拽/nudge)一次提交,单 undo op 全还原。
        let dir = temp_chart_dir();
        let mut doc = ChartDocument::open(&dir).unwrap();
        // fixture line A: notes at beats 1..=4 (indices 0..=3)。
        assert_eq!(note_beats(&doc, 0), vec![1., 2., 3., 4.]);

        // 整体平移 index 1 和 3(非连续),各带独立的 beat/x 变更。
        let n1 = doc.chart().judge_line_list[0].notes.as_ref().unwrap()[1].clone();
        let n3 = doc.chart().judge_line_list[0].notes.as_ref().unwrap()[3].clone();
        let mut m1 = n1.clone();
        m1.start_time = t(2.5);
        m1.end_time = t(2.5);
        m1.position_x = 120.0;
        let mut m3 = n3.clone();
        m3.start_time = t(4.5);
        m3.end_time = t(4.5);
        m3.position_x = -80.0;
        doc.replace_notes_multi(0, &[(1, m1.clone()), (3, m3.clone())]).unwrap();
        assert_eq!(note_beats(&doc, 0), vec![1., 2.5, 3., 4.5]);
        let notes = doc.chart().judge_line_list[0].notes.as_ref().unwrap();
        assert_eq!(notes[1].position_x, 120.0);
        assert_eq!(notes[3].position_x, -80.0);

        // 单次 undo:整批还原(beat + x 全回)。
        assert!(doc.undo());
        assert_eq!(note_beats(&doc, 0), vec![1., 2., 3., 4.]);
        let notes = doc.chart().judge_line_list[0].notes.as_ref().unwrap();
        assert_eq!(notes[1].position_x, 0.0);
        assert_eq!(notes[3].position_x, 0.0);
        assert_eq!(notes[1].start_time.beats(), n1.start_time.beats());
        assert_eq!(notes[3].start_time.beats(), n3.start_time.beats());

        // 单次 redo:整批重放。
        assert!(doc.redo());
        assert_eq!(note_beats(&doc, 0), vec![1., 2.5, 3., 4.5]);
        let notes = doc.chart().judge_line_list[0].notes.as_ref().unwrap();
        assert_eq!(notes[1].position_x, 120.0);
        assert_eq!(notes[3].position_x, -80.0);

        // 索引越界:原子拒绝,不写任何变更、不入栈。
        let mut bad = n1.clone();
        bad.start_time = t(99.);
        assert!(doc.replace_notes_multi(0, &[(0, bad), (99, n1.clone())]).is_err());
        assert_eq!(note_beats(&doc, 0), vec![1., 2.5, 3., 4.5]);
        let notes = doc.chart().judge_line_list[0].notes.as_ref().unwrap();
        assert_eq!(notes[0].start_time.beats(), 1.0); // index 0 未被写入

        // 空批量:no-op,不入栈。
        assert!(doc.replace_notes_multi(0, &[]).is_ok());

        drop(doc);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn add_notes_multi_inserts_sorted_single_undo_op() {
        // PMCORE-XX:复制粘贴整组插入,一次提交 = 单 undo op,全字段还原。
        let dir = temp_chart_dir();
        let mut doc = ChartDocument::open(&dir).unwrap();
        // fixture line A: notes at beats 1..=4 (indices 0..=3)。
        assert_eq!(note_beats(&doc, 0), vec![1., 2., 3., 4.]);

        // 批量含:拍内插值(0.5 / 6.0)与同拍撞车(2.5,落在 note(2.) 后)
        // 的 hold(全字段快照:kind/end/position_x/comment 等)。
        let mut hold = note(2.5);
        hold.kind = 2;
        hold.end_time = t(3.25);
        hold.position_x = 123.0;
        hold.comment = Some("c".into());
        let batch = vec![hold.clone(), note(0.5), note(6.0)];
        doc.add_notes_multi(0, &batch).unwrap();
        // 稳定排序:同拍 2.5 的 hold 插在现有 note(2.) 之后、note(3.) 之前。
        assert_eq!(note_beats(&doc, 0), vec![0.5, 1., 2., 2.5, 3., 4., 6.]);
        let notes = doc.chart().judge_line_list[0].notes.as_ref().unwrap();
        assert_eq!(notes[3].kind, 2);
        assert_eq!(notes[3].end_time.beats(), 3.25);
        assert_eq!(notes[3].position_x, 123.0);
        assert_eq!(notes[3].comment.as_deref(), Some("c"));

        // 单次 undo:整批全部移除,恢复 fixture 原状。
        assert!(doc.undo());
        assert_eq!(note_beats(&doc, 0), vec![1., 2., 3., 4.]);
        let notes = doc.chart().judge_line_list[0].notes.as_ref().unwrap();
        assert_eq!(notes[3].comment, None);

        // 单次 redo:整批重放,全字段一致。
        assert!(doc.redo());
        assert_eq!(note_beats(&doc, 0), vec![0.5, 1., 2., 2.5, 3., 4., 6.]);
        let notes = doc.chart().judge_line_list[0].notes.as_ref().unwrap();
        assert_eq!(notes[3].kind, 2);
        assert_eq!(notes[3].end_time.beats(), 3.25);
        assert_eq!(notes[3].position_x, 123.0);
        assert_eq!(notes[3].comment.as_deref(), Some("c"));

        // line 越界:原子拒绝,不写任何变更、不入栈。
        assert!(doc.add_notes_multi(99, &[note(0.5)]).is_err());
        assert!(doc.add_notes_multi(0, &[]).is_ok()); // 空批量 no-op
        assert_eq!(note_beats(&doc, 0), vec![0.5, 1., 2., 2.5, 3., 4., 6.]);

        // 空批量之后栈未被污染:再 undo 一次仍整批还原。
        assert!(doc.undo());
        assert_eq!(note_beats(&doc, 0), vec![1., 2., 3., 4.]);

        drop(doc);
        fs::remove_dir_all(&dir).ok();
    }

    fn mx_events(d: &ChartDocument) -> &Vec<RPEEvent> {
        d.chart().judge_line_list[0].event_layers[0]
            .as_ref().unwrap().move_x_events.as_ref().unwrap()
    }

    #[test]
    fn events_multi_single_undo_op() {
        // PMCORE-20:事件批量替换(框选平移)与批量删除(框选删除)各为
        // 单 undo op,undo/redo 整体往返。
        let dir = temp_chart_dir();
        let mut doc = ChartDocument::open(&dir).unwrap();
        // 4 个 MoveX 事件(fixture 中该列为空,索引 0..=3 干净)。
        for i in 0..4 {
            let k = doc.add_event(0, 0, EventKind::MoveX, ev(1.0 + i as f64, 1.5 + i as f64, 0.0, 1.0)).unwrap();
            assert_eq!(k, i); // 顺序插入,索引即插入序
        }

        // replace_events_multi:整体 +0.25 平移(保序,不重排)。
        let changes: Vec<(usize, RPEEvent)> = (0..4)
            .map(|i| (i, ev(1.25 + i as f64, 1.75 + i as f64, 0.0, 1.0)))
            .collect();
        doc.replace_events_multi(0, 0, EventKind::MoveX, &changes).unwrap();
        assert_eq!(mx_events(&doc)[0].start_time.beats(), 1.25);
        assert_eq!(mx_events(&doc)[3].start_time.beats(), 4.25);

        // 单次 undo 整批还原;redo 整批重放。
        assert!(doc.undo());
        assert_eq!(mx_events(&doc)[0].start_time.beats(), 1.0);
        assert_eq!(mx_events(&doc)[3].start_time.beats(), 4.0);
        assert!(doc.redo());
        assert_eq!(mx_events(&doc)[0].start_time.beats(), 1.25);
        assert_eq!(mx_events(&doc)[3].start_time.beats(), 4.25);

        // remove_events_multi:删 {0, 2} → 剩 [2.25, 4.25] 两事件(索引不漂移)。
        doc.remove_events_multi(0, 0, EventKind::MoveX, &[0, 2]).unwrap();
        assert_eq!(mx_events(&doc).len(), 2);
        assert_eq!(mx_events(&doc)[0].start_time.beats(), 2.25);
        assert_eq!(mx_events(&doc)[1].start_time.beats(), 4.25);

        // 单次 undo 全数还原(位置与原索引一致);redo 再删。
        assert!(doc.undo());
        assert_eq!(mx_events(&doc).len(), 4);
        assert_eq!(mx_events(&doc)[0].start_time.beats(), 1.25);
        assert_eq!(mx_events(&doc)[2].start_time.beats(), 3.25);
        assert!(doc.redo());
        assert_eq!(mx_events(&doc).len(), 2);
        assert_eq!(mx_events(&doc)[0].start_time.beats(), 2.25);

        // 索引越界:原子拒绝,不写任何变更、不入栈。
        assert!(doc.remove_events_multi(0, 0, EventKind::MoveX, &[0, 99]).is_err());
        assert_eq!(mx_events(&doc).len(), 2);
        // 空批量:no-op。
        assert!(doc.replace_events_multi(0, 0, EventKind::MoveX, &[]).is_ok());
        assert!(doc.remove_events_multi(0, 0, EventKind::MoveX, &[]).is_ok());

        drop(doc);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn new_edit_clears_redo() {
        let dir = temp_chart_dir();
        let mut doc = ChartDocument::open(&dir).unwrap();
        doc.add_note(0, note(2.5)).unwrap();
        assert!(doc.undo());
        assert!(doc.can_redo());
        doc.add_note(0, note(7.)).unwrap();
        assert!(!doc.can_redo());
        drop(doc);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn split_line_partitions_and_remaps() {
        let dir = temp_chart_dir();
        let mut doc = ChartDocument::open(&dir).unwrap();

        let new_index = doc.split_line(0, 2.5).unwrap();
        assert_eq!(new_index, 1);
        assert_eq!(doc.chart().judge_line_list.len(), 4);
        assert_eq!(note_beats(&doc, 0), vec![1., 2.]);
        assert_eq!(note_beats(&doc, 1), vec![3., 4.]);
        assert_eq!(doc.chart().judge_line_list[1].name, "A (split)");

        // crossing alpha event [2,4] values 100->200: midpoint at 2.5 is 125
        let orig_layer = doc.chart().judge_line_list[0].event_layers[0]
            .as_ref()
            .unwrap();
        let alpha = orig_layer.alpha_events.as_ref().unwrap();
        assert_eq!(alpha.len(), 2);
        assert_eq!(alpha[0].end_time.beats(), 1.); // untouched, ends before split
        assert_eq!(alpha[1].end_time.beats(), 2.5);
        assert_eq!(alpha[1].end, 125.);
        let new_layer = doc.chart().judge_line_list[1].event_layers[0]
            .as_ref()
            .unwrap();
        let alpha = new_layer.alpha_events.as_ref().unwrap();
        assert_eq!(alpha.len(), 1);
        assert_eq!(alpha[0].start_time.beats(), 2.5);
        assert_eq!(alpha[0].start, 125.);
        assert_eq!(alpha[0].end, 200.);
        assert_eq!(alpha[0].end_time.beats(), 4.);

        // father remap: C was index 2 fathered to B (1); insertion at 1
        // shifts both: C -> index 3, father -> 2. New line keeps A's father.
        assert_eq!(doc.chart().judge_line_list[3].parent, Some(2));
        assert_eq!(doc.chart().judge_line_list[1].parent, None);

        // undo: merged back, indices restored
        assert!(doc.undo());
        assert_eq!(doc.chart().judge_line_list.len(), 3);
        assert_eq!(note_beats(&doc, 0), vec![1., 2., 3., 4.]);
        assert_eq!(doc.chart().judge_line_list[2].parent, Some(1));

        // redo: split re-applied identically
        assert!(doc.redo());
        assert_eq!(doc.chart().judge_line_list.len(), 4);
        assert_eq!(note_beats(&doc, 1), vec![3., 4.]);

        drop(doc);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn bind_lines_merges_and_remaps() {
        let dir = temp_chart_dir();
        let mut doc = ChartDocument::open(&dir).unwrap();

        // bind B (1) into A (0)
        doc.bind_lines(0, 1).unwrap();
        assert_eq!(doc.chart().judge_line_list.len(), 2);
        assert_eq!(note_beats(&doc, 0), vec![1., 2., 3., 4., 5., 6.]);
        // A's speed list (was Some([])) merged B's [0,8] speed event
        let layer = doc.chart().judge_line_list[0].event_layers[0]
            .as_ref()
            .unwrap();
        assert_eq!(layer.speed_events.as_ref().unwrap().len(), 1);
        // C (now at 1) was fathered to source (1) -> -1
        assert_eq!(doc.chart().judge_line_list[1].parent, Some(-1));

        // undo: B restored at index 1 with content, A truncated, C re-fathered
        assert!(doc.undo());
        assert_eq!(doc.chart().judge_line_list.len(), 3);
        assert_eq!(note_beats(&doc, 0), vec![1., 2., 3., 4.]);
        assert_eq!(note_beats(&doc, 1), vec![5., 6.]);
        assert_eq!(doc.chart().judge_line_list[1].name, "B");
        assert_eq!(doc.chart().judge_line_list[2].parent, Some(1));
        let layer = doc.chart().judge_line_list[0].event_layers[0]
            .as_ref()
            .unwrap();
        assert_eq!(layer.speed_events.as_ref().unwrap().len(), 0);

        // redo: merged again
        assert!(doc.redo());
        assert_eq!(doc.chart().judge_line_list.len(), 2);
        assert_eq!(note_beats(&doc, 0), vec![1., 2., 3., 4., 5., 6.]);

        drop(doc);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn add_remove_line_undo_redo_roundtrip() {
        let dir = temp_chart_dir();
        let mut doc = ChartDocument::open(&dir).unwrap();

        // add: appended at the end with z-order above the current top
        let idx = doc.add_line("D".into(), "custom.png".into()).unwrap();
        assert_eq!(idx, 3);
        assert_eq!(doc.chart().judge_line_list[3].name, "D");
        assert_eq!(doc.chart().judge_line_list[3].texture, "custom.png");
        assert_eq!(doc.chart().judge_line_list[3].z_order, 1);
        assert!(doc.undo());
        assert_eq!(doc.chart().judge_line_list.len(), 3);
        assert!(doc.redo());
        assert_eq!(doc.chart().judge_line_list.len(), 4);

        // remove: father remap — C (fathered to B at 1) becomes fatherless
        let removed = doc.remove_line(1).unwrap();
        assert_eq!(removed.name, "B");
        assert_eq!(doc.chart().judge_line_list.len(), 3);
        assert_eq!(doc.chart().judge_line_list[1].name, "C");
        assert_eq!(doc.chart().judge_line_list[1].parent, Some(-1));

        // undo: B restored, C re-fathered
        assert!(doc.undo());
        assert_eq!(doc.chart().judge_line_list.len(), 4);
        assert_eq!(doc.chart().judge_line_list[1].name, "B");
        assert_eq!(doc.chart().judge_line_list[2].parent, Some(1));

        // redo: removed again, remap identical
        assert!(doc.redo());
        assert_eq!(doc.chart().judge_line_list.len(), 3);
        assert_eq!(doc.chart().judge_line_list[1].parent, Some(-1));

        // cannot remove the last line
        while doc.chart().judge_line_list.len() > 1 {
            doc.remove_line(0).unwrap();
        }
        assert!(doc.remove_line(0).is_err());

        drop(doc);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn bpm_ops_undo_redo_roundtrip() {
        let dir = temp_chart_dir();
        let mut doc = ChartDocument::open(&dir).unwrap();
        assert_eq!(doc.chart().bpm_list.len(), 1);

        // add: sorted insertion between the fixture's 120 BPM
        let idx = doc.add_bpm(160.0, 4.0).unwrap();
        assert_eq!(idx, 1);
        let idx = doc.add_bpm(90.0, 2.0).unwrap();
        assert_eq!(idx, 1);
        assert_eq!(doc.chart().bpm_list[1].bpm, 90.0);
        assert_eq!(doc.chart().bpm_list[2].bpm, 160.0);

        // undo both, redo both
        assert!(doc.undo());
        assert_eq!(doc.chart().bpm_list.len(), 2);
        assert_eq!(doc.chart().bpm_list[1].bpm, 160.0);
        assert!(doc.undo());
        assert_eq!(doc.chart().bpm_list.len(), 1);
        assert!(doc.redo());
        assert!(doc.redo());
        assert_eq!(doc.chart().bpm_list.len(), 3);
        assert_eq!(doc.chart().bpm_list[1].bpm, 90.0);
        assert_eq!(doc.chart().bpm_list[2].bpm, 160.0);

        // replace roundtrip
        let old = doc.replace_bpm(1, 100.0, 2.5).unwrap();
        assert_eq!(old.bpm, 90.0);
        assert_eq!(doc.chart().bpm_list[1].bpm, 100.0);
        assert_eq!(doc.chart().bpm_list[1].start_time.beats(), 2.5);
        assert!(doc.undo());
        assert_eq!(doc.chart().bpm_list[1].bpm, 90.0);
        assert!(doc.redo());
        assert_eq!(doc.chart().bpm_list[1].bpm, 100.0);

        // remove roundtrip
        let removed = doc.remove_bpm(1).unwrap();
        assert_eq!(removed.bpm, 100.0);
        assert!(doc.undo());
        assert_eq!(doc.chart().bpm_list.len(), 3);
        assert!(doc.redo());
        assert_eq!(doc.chart().bpm_list.len(), 2);

        // cannot remove the last BPM
        while doc.chart().bpm_list.len() > 1 {
            doc.remove_bpm(0).unwrap();
        }
        assert!(doc.remove_bpm(0).is_err());

        drop(doc);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn save_open_roundtrip() {
        let dir = temp_chart_dir();
        let mut doc = ChartDocument::open(&dir).unwrap();
        doc.add_note(0, note(2.5)).unwrap();

        // sync save
        doc.save().unwrap();
        assert!(!doc.is_dirty());
        let doc2 = ChartDocument::open(&dir).unwrap();
        assert_eq!(note_beats(&doc2, 0), vec![1., 2., 2.5, 3., 4.]);
        drop(doc2);

        // background save + flush
        doc.add_note(0, note(3.5)).unwrap();
        assert!(doc.is_dirty());
        doc.save_background();
        assert!(!doc.is_dirty());
        doc.flush().unwrap();
        let doc3 = ChartDocument::open(&dir).unwrap();
        assert_eq!(note_beats(&doc3, 0), vec![1., 2., 2.5, 3., 3.5, 4.]);
        drop(doc3);

        // debounced write happens within ~1s without an explicit flush
        doc.add_note(0, note(7.5)).unwrap();
        doc.save_background();
        std::thread::sleep(Duration::from_millis(1500));
        let doc4 = ChartDocument::open(&dir).unwrap();
        assert_eq!(note_beats(&doc4, 0), vec![1., 2., 2.5, 3., 3.5, 4., 7.5]);
        drop(doc4);

        drop(doc);
        fs::remove_dir_all(&dir).ok();
    }

    /// PMCORE-19:undo/redo 后显式 save() 落盘内容与内存一致(重开验证);
    /// undo/redo 均置 dirty(触发保存的判定依据)。自动保存联动由
    /// `autosave_bak_and_flush` 覆盖。
    #[test]
    fn undo_redo_save_reopen_matches_memory() {
        let dir = temp_chart_dir();
        let mut doc = ChartDocument::open(&dir).unwrap();
        doc.add_note(0, note(2.5)).unwrap();
        assert_eq!(note_beats(&doc, 0), vec![1., 2., 2.5, 3., 4.]);

        // undo → dirty → save → 重开:回到 fixture(与内存一致)。
        assert!(doc.undo());
        assert!(doc.is_dirty(), "undo 置 dirty");
        assert_eq!(note_beats(&doc, 0), vec![1., 2., 3., 4.]);
        doc.save().unwrap();
        assert!(!doc.is_dirty());
        let doc2 = ChartDocument::open(&dir).unwrap();
        assert_eq!(note_beats(&doc2, 0), vec![1., 2., 3., 4.]);
        drop(doc2);

        // redo → dirty → save → 重开:音符回来(与内存一致)。
        assert!(doc.redo());
        assert!(doc.is_dirty(), "redo 置 dirty");
        assert_eq!(note_beats(&doc, 0), vec![1., 2., 2.5, 3., 4.]);
        doc.save().unwrap();
        let doc3 = ChartDocument::open(&dir).unwrap();
        assert_eq!(note_beats(&doc3, 0), vec![1., 2., 2.5, 3., 4.]);
        drop(doc3);

        drop(doc);
        fs::remove_dir_all(&dir).ok();
    }

    /// PMCORE-77:注释 CRUD + 保存→重开仍在;None = 删除;越界报错。
    #[test]
    fn comment_crud_save_reopen_roundtrip() {
        let dir = temp_chart_dir();
        let mut doc = ChartDocument::open(&dir).unwrap();

        // 初始无注释。
        assert_eq!(doc.note_comment(0, 0), None);
        assert_eq!(doc.line_comment(0), None);

        // 设音符注释 + 判定线注释。
        doc.set_note_comment(0, 0, Some("这个 tap 要重一点".into())).unwrap();
        doc.set_note_comment(0, 2, Some("hold 起点".into())).unwrap();
        doc.set_line_comment(0, Some("第一段配置".into())).unwrap();
        assert_eq!(doc.note_comment(0, 0), Some("这个 tap 要重一点"));
        assert_eq!(doc.note_comment(0, 2), Some("hold 起点"));
        assert_eq!(doc.line_comment(0), Some("第一段配置"));
        assert!(doc.is_dirty());

        // 越界报错。
        assert!(doc.set_note_comment(0, 99, Some("x".into())).is_err());
        assert!(doc.set_line_comment(99, Some("x".into())).is_err());

        // 保存 → 重开仍在。
        doc.save().unwrap();
        let doc2 = ChartDocument::open(&dir).unwrap();
        assert_eq!(doc2.note_comment(0, 0), Some("这个 tap 要重一点"));
        assert_eq!(doc2.note_comment(0, 2), Some("hold 起点"));
        assert_eq!(doc2.line_comment(0), Some("第一段配置"));
        drop(doc2);

        // None = 删除;重开后消失,其余保留。
        doc.set_note_comment(0, 0, None).unwrap();
        doc.set_line_comment(0, None).unwrap();
        assert_eq!(doc.note_comment(0, 0), None);
        assert_eq!(doc.line_comment(0), None);
        doc.save().unwrap();
        let doc3 = ChartDocument::open(&dir).unwrap();
        assert_eq!(doc3.note_comment(0, 0), None);
        assert_eq!(doc3.note_comment(0, 2), Some("hold 起点"));
        assert_eq!(doc3.line_comment(0), None);
        drop(doc3);

        drop(doc);
        fs::remove_dir_all(&dir).ok();
    }

    /// PMCORE-77:无注释谱面 serde 往返字节零差异;带注释往返幂等。
    #[test]
    fn comment_field_roundtrip_byte_identical() {
        let chart = fixture_chart(); // 全谱无注释
        let bytes1 = serde_json::to_vec(&chart).unwrap();
        // 反序列化 → 再序列化:comment=None 不写字段,字节必须一致。
        let parsed: RPEChart = serde_json::from_slice(&bytes1).unwrap();
        let bytes2 = serde_json::to_vec(&parsed).unwrap();
        assert_eq!(bytes1, bytes2, "无注释谱面往返必须字节一致");

        // 带注释:写 → 读回 → 再写,幂等;注释值保留。
        let mut with_c = chart.clone();
        with_c.judge_line_list[0].comment = Some("line note".into());
        with_c.judge_line_list[0].notes.as_mut().unwrap()[1].comment = Some("note note".into());
        let c1 = serde_json::to_vec(&with_c).unwrap();
        let parsed2: RPEChart = serde_json::from_slice(&c1).unwrap();
        assert_eq!(parsed2.judge_line_list[0].comment.as_deref(), Some("line note"));
        assert_eq!(parsed2.judge_line_list[0].notes.as_ref().unwrap()[1].comment.as_deref(), Some("note note"));
        let c2 = serde_json::to_vec(&parsed2).unwrap();
        assert_eq!(c1, c2, "带注释谱面往返必须字节一致(幂等)");
    }

    /// PMCORE-77:注释挂 note 上——音符替换(拖拽移动)后注释跟随;
    /// 音符删除后注释随字段丢失。
    #[test]
    fn comment_anchors_to_note_not_index() {
        let dir = temp_chart_dir();
        let mut doc = ChartDocument::open(&dir).unwrap();
        doc.set_note_comment(0, 1, Some("被拖走".into())).unwrap();

        // 替换音符(拖拽移动):注释随 note 数据保留(索引语义不变)。
        let mut moved = doc.chart().judge_line_list[0].notes.as_ref().unwrap()[1].clone();
        moved.start_time = t(9.0);
        doc.replace_note(0, 1, moved).unwrap();
        assert_eq!(doc.note_comment(0, 1), Some("被拖走"));

        // 删除音符:注释随字段丢失。
        doc.remove_note(0, 1).unwrap();
        assert_eq!(doc.note_comment(0, 1), None);
        assert_eq!(doc.note_comment(0, 0), None);
        drop(doc);
        fs::remove_dir_all(&dir).ok();
    }

    /// PMCORE-24:自动保存 + .bak + 退出 flush。
    #[test]
    fn autosave_bak_and_flush() {
        let dir = temp_chart_dir();
        let mut doc = ChartDocument::open(&dir).unwrap();
        let chart_path = dir.join("chart.json");
        let bak_path = dir.join("chart.json.bak");
        let read_beats = |p: &std::path::Path| -> Vec<f64> {
            let ch: RPEChart = serde_json::from_slice(&fs::read(p).unwrap()).unwrap();
            ch.judge_line_list[0].notes.as_ref().unwrap().iter()
                .map(|n| n.start_time.beats()).collect()
        };

        // 自动保存关:编辑只置 dirty,不落盘(与改动前行为一致)。
        doc.set_autosave(false, 100);
        doc.add_note(0, note(2.5)).unwrap();
        assert!(doc.is_dirty(), "autosave off: edit keeps dirty");
        std::thread::sleep(Duration::from_millis(200));
        assert_eq!(read_beats(&chart_path), vec![1., 2., 3., 4.], "autosave off: file untouched");
        assert!(!bak_path.exists(), "autosave off: no .bak yet");
        assert!(doc.undo()); // 清掉这条 note,回到基线
        assert!(doc.is_dirty(), "undo with autosave off also keeps dirty");

        // 自动保存开:编辑后自动 save_background,防抖落盘——全程不调用
        // save()/save_background()/flush(),模拟"编辑完直接强杀进程"。
        doc.set_autosave(true, 100);
        doc.add_note(0, note(2.5)).unwrap();
        doc.add_note(0, note(3.5)).unwrap();
        doc.add_note(0, note(1.5)).unwrap();
        assert!(!doc.is_dirty(), "autosave on: save intent acknowledged");
        std::thread::sleep(Duration::from_millis(350)); // > 防抖间隔
        assert_eq!(read_beats(&chart_path), vec![1., 1.5, 2., 2.5, 3., 3.5, 4.], "autosaved without explicit save");
        assert!(bak_path.is_file(), ".bak exists after first replace");

        // .bak = 上一次成功写入的版本。防抖是限速语义(两次写入最多隔
        // interval),快速连击时中间快照也可能落盘,所以 .bak 可能是任意
        // 中间版本——契约是:是合法旧版本(音符 ⊆ 最终主文件)、且 ≠ 主文件。
        let bak_beats = read_beats(&bak_path);
        let main_beats = read_beats(&chart_path);
        assert!(
            bak_beats.iter().all(|b| main_beats.contains(b)),
            ".bak is an older version of the chart"
        );
        assert_ne!(bak_beats, main_beats, ".bak differs from the final main file");

        // 正常保存后主文件比 .bak 新——崩溃恢复检测(mtime 对比)的依据。
        let mt = fs::metadata(&chart_path).unwrap().modified().unwrap();
        let bt = fs::metadata(&bak_path).unwrap().modified().unwrap();
        assert!(mt >= bt, "after a successful save main file is newer than .bak");

        // undo 同样触发自动保存(编辑语义一致)。
        assert!(doc.undo()); // 移除最后一条(1.5)
        assert!(!doc.is_dirty(), "undo with autosave on also saves");
        doc.flush().unwrap();
        assert_eq!(read_beats(&chart_path), vec![1., 2., 2.5, 3., 3.5, 4.]);
        assert_eq!(read_beats(&bak_path), vec![1., 1.5, 2., 2.5, 3., 3.5, 4.], ".bak rolls forward");

        drop(doc);
        fs::remove_dir_all(&dir).ok();
    }

    // ---------------------------------------------------------------------
    // PMCORE-23:info 元数据编辑 + 写回
    // ---------------------------------------------------------------------

    fn read_info_file(dir: &Path) -> String {
        fs::read_to_string(dir.join("info.json")).unwrap()
    }

    #[test]
    fn info_edit_json_roundtrip_and_no_side_effect() {
        let dir = temp_chart_dir();
        let before = read_info_file(&dir);
        let mut doc = ChartDocument::open(&dir).unwrap();
        assert!(!doc.info_dirty());

        // 干净会话 save():info 文件不产生改动(无副作用验收)。
        doc.save().unwrap();
        assert_eq!(read_info_file(&dir), before, "clean save leaves info.json untouched");

        // 改字段 → info_dirty;不入 undo 栈、不置 chart dirty。
        doc.set_info_field(InfoField::Name, "新名字".into()).unwrap();
        doc.set_info_field(InfoField::Level, "IN Lv.15".into()).unwrap();
        doc.set_info_field(InfoField::Difficulty, "12.4".into()).unwrap();
        assert!(doc.info_dirty());
        assert!(!doc.is_dirty(), "info 修改不置 chart dirty");
        assert!(!doc.can_undo(), "info 修改不入 undo 栈");

        // difficulty clamp ≥ 0;非数字报错。
        doc.set_info_field(InfoField::Difficulty, "-3".into()).unwrap();
        assert_eq!(doc.info().difficulty, 0.0);
        assert!(doc.set_info_field(InfoField::Difficulty, "abc".into()).is_err());

        // save → info.json 更新;重开谱面值一致。
        doc.save().unwrap();
        assert!(!doc.info_dirty());
        let doc2 = ChartDocument::open(&dir).unwrap();
        assert_eq!(doc2.info().name, "新名字");
        assert_eq!(doc2.info().level, "IN Lv.15");
        assert_eq!(doc2.info().difficulty, 0.0);
        assert_eq!(doc2.info().chart, "chart.json", "chart 路径字段不被触碰");
        drop(doc2);

        drop(doc);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn info_yml_writeback_roundtrip() {
        let dir = std::env::temp_dir().join(format!(
            "phimakor-edit-yml-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&dir).unwrap();
        // 最小 info.yml(RPE web 导出风格,字段可缺省)。
        fs::write(
            dir.join("info.yml"),
            "name: yml-fixture\nchart: chart.json\ndifficulty: 10.5\n",
        )
        .unwrap();
        fs::write(
            dir.join("chart.json"),
            serde_json::to_string(&fixture_chart()).unwrap(),
        )
        .unwrap();

        let mut doc = ChartDocument::open(&dir).unwrap();
        assert_eq!(doc.info().name, "yml-fixture");
        doc.set_info_field(InfoField::Name, "yml-v2".into()).unwrap();
        doc.set_info_field(InfoField::Composer, "NewComposer".into()).unwrap();
        doc.save().unwrap();
        assert!(!dir.join("info.json").exists(), "yml 源不产生 info.json");
        let yml = fs::read_to_string(dir.join("info.yml")).unwrap();
        assert!(yml.contains("name: yml-v2"), "yml 写回字段更新");
        assert!(yml.contains("composer: NewComposer"));
        // 字段命名保持(InfoYaml 显式 rename 的 camelCase 键)。
        assert!(yml.contains("previewStart:"), "yml camelCase 键保持");
        assert!(yml.contains("aspectRatio:"));
        // 可被 load_info 读回。
        let info = crate::core::chart::load_info(&dir).unwrap();
        assert_eq!(info.name, "yml-v2");
        assert_eq!(info.composer, "NewComposer");
        assert_eq!(info.chart, "chart.json");

        drop(doc);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn info_txt_upgrades_to_json_on_save() {
        let dir = std::env::temp_dir().join(format!(
            "phimakor-edit-txt-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("info.txt"),
            "Name: txt-fixture\nChart: chart.json\nLevel: AT Lv.16\n",
        )
        .unwrap();
        fs::write(
            dir.join("chart.json"),
            serde_json::to_string(&fixture_chart()).unwrap(),
        )
        .unwrap();

        let mut doc = ChartDocument::open(&dir).unwrap();
        assert_eq!(doc.info().name, "txt-fixture");
        doc.set_info_field(InfoField::Name, "txt-upgraded".into()).unwrap();
        doc.set_info_field(InfoField::Level, "AT Lv.16".into()).unwrap();
        doc.save().unwrap();
        assert!(dir.join("info.json").is_file(), "txt 源升级写 info.json");
        // info.json 可读回,值一致。
        let doc2 = ChartDocument::open(&dir).unwrap();
        assert_eq!(doc2.info().name, "txt-upgraded");
        assert_eq!(doc2.info().level, "AT Lv.16");
        assert_eq!(doc2.info().chart, "chart.json");
        drop(doc2);

        drop(doc);
        fs::remove_dir_all(&dir).ok();
    }
}
