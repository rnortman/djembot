//! The window: the lesson's segments stacked in one scroll area, each rhythm
//! block drawn as a grid and a necklace with its own play button, and the
//! UI thread's side of the socket.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

use anyhow::{Context, Result};
use eframe::egui::{
    self, Align, Align2, Color32, FontId, Key, Pos2, Rect, RichText, Sense, Stroke, Vec2,
};
use egui_commonmark::{CommonMarkCache, CommonMarkViewer};

use crate::engine::{self, ClockSnapshot, Engine, Mono, Position, heard};
use crate::input::{self, Arming, Hit, Part, Shared, Trigger, lock};
use crate::ipc::{Event, EventKind, Pending, Request, Response};
use crate::lesson::{self, Lesson, Segment};
use crate::model::{Diagnostics, Exercise, MAX_VOICES, Next};
use crate::perf::{
    self, Calibration, EndedBy, Mark, MarkKind, Neighbours, Performance, Run, Tally, Timing,
};
use crate::sounds::{Bank, SoundId};
use crate::timeline::Timeline;

const ROW_HEIGHT: f32 = 26.0;
/// Height of the beat-number strip above the grid rows.
const BEAT_STRIP: f32 = 14.0;
const LABEL_WIDTH: f32 = 90.0;
const GAP: f32 = 12.0;
/// How long an onset stays lit after the playhead passes it.
const FLASH_SECS: f64 = 0.150;
const MUTED_ALPHA: f32 = 0.3;
/// Judgments of earlier cycles are drawn this faint, for this many cycles.
const PAST_ALPHA: f32 = 0.25;
const HISTORY_CYCLES: i64 = 32;
const ON_COLOR: Color32 = Color32::from_rgb(0x40, 0xc0, 0x40);
const EARLY_COLOR: Color32 = Color32::from_rgb(0x40, 0x80, 0xff);
const LATE_COLOR: Color32 = Color32::from_rgb(0xff, 0x90, 0x20);
const WRONG_COLOR: Color32 = Color32::from_rgb(0xe0, 0x30, 0x30);
const REPAINT: Duration = Duration::from_millis(16);
/// How long the window's focus counts for the input hub, from a pass that
/// expects another within `REPAINT`.
const FOCUS_STALE: Duration = Duration::from_millis(250);
const PALETTE: [Color32; MAX_VOICES] = [
    Color32::from_rgb(0xe0, 0x6c, 0x3c),
    Color32::from_rgb(0x3c, 0x9a, 0xe0),
    Color32::from_rgb(0x5c, 0xb8, 0x5c),
    Color32::from_rgb(0xc8, 0x5c, 0xc8),
    Color32::from_rgb(0xd8, 0xb0, 0x30),
    Color32::from_rgb(0x40, 0xc0, 0xb0),
    Color32::from_rgb(0xe0, 0x50, 0x70),
    Color32::from_rgb(0x90, 0x90, 0xe0),
    Color32::from_rgb(0xa0, 0xa0, 0xa0),
];

/// The requests waiting on `rx` whose deadline has not passed. An expired
/// request is dropped unanswered: its client has already been told the
/// window did not respond, so acting on it now would change the window
/// behind the instructor's back.
pub fn drain(rx: &mpsc::Receiver<Pending>) -> Vec<Pending> {
    rx.try_iter().filter(|p| !p.expired()).collect()
}

/// The block (0-based) that `label` names in `lesson`, the first block when
/// `label` is `None`; otherwise the error the client is sent.
pub fn pick_block(lesson: Option<&Lesson>, label: Option<&str>) -> Result<usize, String> {
    let lesson = lesson.ok_or("no lesson loaded; run `djembot show LESSON.md` first")?;
    let labels = lesson.labels();
    if labels.is_empty() {
        return Err("the lesson has no rhythm blocks".into());
    }
    match label {
        None => Ok(0),
        Some(key) => lesson.find(key).ok_or_else(|| {
            format!(
                "unknown label \"{key}\"; blocks: {}",
                labels
                    .iter()
                    .enumerate()
                    .map(|(i, l)| format!("{} {l}", i + 1))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        }),
    }
}

/// Brightness in `[0, 1]` of an onset at `onset_frac` of the cycle, given
/// the playhead at `pos` and a cycle lasting `cycle_secs`: 1 the moment the
/// playhead passes it, fading to 0 over `FLASH_SECS`.
pub fn flash(pos: Position, onset_frac: f64, cycle_secs: f64) -> f32 {
    let mut since = pos.frac - onset_frac;
    if since < 0.0 {
        if pos.cycle == 0 {
            return 0.0;
        }
        since += 1.0;
    }
    let secs = since * cycle_secs;
    if secs >= FLASH_SECS {
        0.0
    } else {
        (1.0 - secs / FLASH_SECS) as f32
    }
}

/// The link of `chain` the mixer frame `at` falls in: the last whose origin
/// is not after it, else the head.
fn link_at(chain: &[Link], at: f64) -> Option<usize> {
    let last = chain.iter().rposition(|l| l.origin as f64 <= at);
    last.or((!chain.is_empty()).then_some(0))
}

/// A score compiled for the engine: its links and the head's parts.
struct Score {
    chain: Vec<Link>,
    arming: Option<Arming>,
}

impl Score {
    /// The commands `App::load` and a `Play` send for it.
    fn sends(&self) -> usize {
        if self.chain.len() > 1 { 3 } else { 2 }
    }
}

/// The blocks of the score that starts at `block`, with their indices: it
/// and each block that follows attacca, up to the first whose `next` is
/// `stop`.
fn score(lesson: &Lesson, block: usize) -> impl Iterator<Item = (usize, &Exercise)> {
    lesson
        .blocks()
        .enumerate()
        .skip(block)
        .scan(true, |more, (i, ex)| {
            (*more).then(|| {
                *more = ex.next == Next::Attacca;
                (i, ex)
            })
        })
}

/// Whether any block of the score that starts at `block` gives the user a
/// part.
fn score_has_part(lesson: &Lesson, block: usize) -> bool {
    score(lesson, block).any(|(_, ex)| ex.has_part())
}

/// Compiles the score from `block` of `doc` at `rate`, each link with its
/// block's mutes. A link after the head is entered attacca, so it is
/// compiled without its count-in; each link's origin is where the one
/// before it ends, and its neighbours are the beats of the links on either
/// side.
fn compile_score(bank: &mut Bank, rate: u32, doc: &Doc, block: usize) -> Result<Score> {
    let mut chain: Vec<Link> = Vec::new();
    let mut origin = 0;
    for (i, (b, ex)) in score(&doc.lesson, block).enumerate() {
        let mut ex = ex.clone();
        if i > 0 {
            ex.count_in = 0;
        }
        let mut tl = Timeline::from(&ex, bank, rate)?;
        tl.mute = doc.blocks[b].muted;
        let end = tl.end.unwrap_or(0);
        let beat = perf::beat_frames(&ex, &tl);
        let before = chain.last_mut().map(|prev| {
            prev.neighbours.after = Some(beat);
            perf::beat_frames(&prev.ex, &prev.tl)
        });
        chain.push(Link {
            block: b,
            ex,
            tl: Arc::new(tl),
            origin,
            neighbours: Neighbours {
                before,
                after: None,
            },
        });
        origin += end;
    }
    let head = doc.block(block).context("no such block")?;
    Ok(Score {
        chain,
        arming: arming([head], bank)?,
    })
}

/// The parts of the player voices of `exs` by trigger, each with the
/// voice's unaccented sound at its tone, or no sound for a silent voice; a
/// trigger bound in several takes the last one's part. `None` when none has
/// a player voice.
pub fn arming<'a>(
    exs: impl IntoIterator<Item = &'a Exercise>,
    bank: &mut Bank,
) -> Result<Option<Arming>> {
    let mut parts = HashMap::new();
    for ex in exs {
        for (trigger, v, tone) in bindings(ex) {
            let voice = &ex.voices[v];
            let buf = if voice.silent {
                None
            } else {
                Some(bank.get(SoundId {
                    instrument: voice.instrument,
                    tone,
                    accent: false,
                })?)
            };
            let part = Part {
                buf,
                gain: voice.gain,
                pan: voice.pan,
            };
            parts.insert(trigger, part);
        }
    }
    Ok((!parts.is_empty()).then_some(Arming { parts }))
}

/// Every key binding of `ex`'s player voices: its trigger, the voice and
/// the tone it plays.
fn bindings(ex: &Exercise) -> impl Iterator<Item = (Trigger, usize, char)> + '_ {
    ex.player_voices().flat_map(move |v| {
        ex.voices[v].keys.iter().map(move |binding| {
            let code = input::keys::code(&binding.key)
                .expect("an exercise's key names are validated by `model::from_value`");
            (Trigger::Key(code), v, binding.tone)
        })
    })
}

/// The voice and tone each trigger plays in `ex`'s player voices.
fn part_keys(ex: &Exercise) -> HashMap<Trigger, (usize, char)> {
    bindings(ex)
        .map(|(trigger, v, tone)| (trigger, (v, tone)))
        .collect()
}

/// `you: clave (f)` for a block with a player part; a part on an
/// instrument with several tones lists key and tone, `conga (f o · g s ·
/// v b)`. `None` without a player part.
pub fn you_text(ex: &Exercise) -> Option<String> {
    let parts: Vec<String> = ex
        .player_voices()
        .map(|i| {
            let v = &ex.voices[i];
            let tones = v.instrument.tones().len() > 1;
            let keys: Vec<String> = v
                .keys
                .iter()
                .map(|k| {
                    if tones {
                        format!("{} {}", k.key, k.tone)
                    } else {
                        k.key.clone()
                    }
                })
                .collect();
            format!("{} ({})", v.name, keys.join(" · "))
        })
        .collect();
    (!parts.is_empty()).then(|| format!("you: {}", parts.join(", ")))
}

/// A performance's line in its block header: for the calibration block
/// its progress towards `perf::CALIBRATION_TAPS`, `taps 12/24 · median
/// -112 ms` (no median before the first hit), else the tally.
pub fn performance_text(perf: &Performance) -> String {
    if !perf.is_calibration() {
        return tally_text(perf.tally());
    }
    let taps = format!("taps {}/{}", perf.hit_count(), perf::CALIBRATION_TAPS);
    match perf.median_ms() {
        Some(m) => format!("{taps} · median {m:+.0} ms"),
        None => taps,
    }
}

/// The status line's audio segment: `audio: 128 frames · 9 ms`, the frames
/// per callback and the device-reported output latency; `audio: –` before
/// the device has reported them.
pub fn audio_text(status: Option<(u32, Duration)>) -> String {
    match status {
        Some((period, latency)) => format!(
            "audio: {period} frames · {:.0} ms",
            latency.as_secs_f64() * 1000.0
        ),
        None => "audio: –".into(),
    }
}

/// `hit 24 · miss 2 · extra 1 · mean +8 ms`; no mean before the first hit.
pub fn tally_text(t: Tally) -> String {
    let counts = format!("hit {} · miss {} · extra {}", t.hit, t.miss, t.extra);
    match t.mean_ms {
        Some(m) => format!("{counts} · mean {m:+.0} ms"),
        None => counts,
    }
}

/// The judgments of `perf` to draw, each with its alpha, faintest first so
/// the current cycle's are drawn on top: the current cycle's (and any
/// already in the next) at full alpha, the `HISTORY_CYCLES` before it at
/// `PAST_ALPHA`, older ones not at all.
pub fn visible_marks(perf: &Performance) -> Vec<(Mark, f32)> {
    let current = perf.current_cycle() as i64;
    let mut marks: Vec<(Mark, f32)> = perf
        .marks_since(current - HISTORY_CYCLES)
        .into_iter()
        .filter_map(|m| match current - m.cycle {
            age if age <= 0 => Some((m, 1.0)),
            age if age <= HISTORY_CYCLES => Some((m, PAST_ALPHA)),
            _ => None,
        })
        .collect();
    marks.sort_by(|a, b| a.1.total_cmp(&b.1));
    marks
}

fn mark_color(kind: MarkKind) -> Color32 {
    match kind {
        MarkKind::Hit(Timing::On) => ON_COLOR,
        MarkKind::Hit(Timing::Early) => EARLY_COLOR,
        MarkKind::Hit(Timing::Late) => LATE_COLOR,
        MarkKind::Miss | MarkKind::Extra => WRONG_COLOR,
    }
}

/// A judgment at `at`: a dot for a hit, a ring of `ring` radius for a
/// miss (drawn around the target), a cross for an extra.
fn paint_mark(painter: &egui::Painter, at: Pos2, kind: MarkKind, alpha: f32, ring: f32) {
    let color = mark_color(kind).gamma_multiply(alpha);
    match kind {
        MarkKind::Hit(_) => {
            let outline = Stroke::new(1.0, Color32::BLACK.gamma_multiply(alpha));
            painter.circle(at, 3.5, color, outline);
        }
        MarkKind::Miss => {
            painter.circle_stroke(at, ring, Stroke::new(2.0, color));
        }
        MarkKind::Extra => {
            let s = 3.5;
            let stroke = Stroke::new(2.0, color);
            painter.line_segment([at + Vec2::new(-s, -s), at + Vec2::new(s, s)], stroke);
            painter.line_segment([at + Vec2::new(-s, s), at + Vec2::new(s, -s)], stroke);
        }
    }
}

/// `90 bpm · 4 beats · 8 cycles`, or `∞` for a looping exercise; then
/// `· then B` when block `then` follows it attacca.
pub fn meta_line(ex: &Exercise, then: Option<&str>) -> String {
    let cycles = match ex.cycles {
        Some(1) => "1 cycle".to_string(),
        Some(n) => format!("{n} cycles"),
        None => "∞".to_string(),
    };
    let beats = if ex.beats == 1 { "beat" } else { "beats" };
    let then = then.map_or_else(String::new, |b| format!(" · then {b}"));
    format!(
        "{} bpm · {} {beats} · {cycles}{then}",
        fmt_num(ex.tempo),
        ex.beats
    )
}

fn fmt_num(x: f64) -> String {
    if x.fract() == 0.0 {
        format!("{x:.0}")
    } else {
        format!("{x}")
    }
}

/// The count-in beats heard so far: `1 · 2` while the second is heard.
pub fn count_in_text(beat: u32) -> String {
    (1..=beat)
        .map(|n| n.to_string())
        .collect::<Vec<_>>()
        .join(" · ")
}

/// What the transport is doing, for the status line.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Transport {
    Stopped,
    /// Loaded and waiting for the user to start it.
    Armed,
    Paused,
    CountIn,
    Playing(Position),
}

/// `playing A · cycle 3/8`, `playing A · cycle 3 ∞`, `count-in A`,
/// `paused A`, `armed A`, or `stopped`.
pub fn transport_text(label: Option<&str>, cycles: Option<u32>, t: Transport) -> String {
    let label = label.unwrap_or("");
    match t {
        Transport::Stopped => "stopped".into(),
        Transport::Armed => format!("armed {label}"),
        Transport::Paused => format!("paused {label}"),
        Transport::CountIn => format!("count-in {label}"),
        Transport::Playing(p) => match cycles {
            Some(n) => format!("playing {label} · cycle {}/{n}", p.cycle + 1),
            None => format!("playing {label} · cycle {} ∞", p.cycle + 1),
        },
    }
}

/// The lesson on screen.
struct Doc {
    path: String,
    lesson: Lesson,
    /// Each block's display state, in block order; kept across block
    /// switches.
    blocks: Vec<BlockState>,
    /// The block currently loaded into the engine.
    active: Option<usize>,
    /// The block the keys act on, drawn highlighted. A valid index when the
    /// lesson has blocks; becomes the active block whenever one is loaded.
    selected: usize,
}

impl Doc {
    fn new(path: String, lesson: Lesson) -> Doc {
        let blocks = lesson
            .blocks()
            .map(|ex| BlockState {
                muted: ex.initial_mutes(),
                hidden: ex.hidden,
            })
            .collect();
        Doc {
            path,
            lesson,
            blocks,
            active: None,
            selected: 0,
        }
    }

    fn block(&self, i: usize) -> Option<&Exercise> {
        self.lesson.blocks().nth(i)
    }
}

/// Where the window keeps its files; `None` where there is no home
/// directory to put them in.
#[derive(Clone, Debug, Default)]
pub struct Store {
    /// The directory run files are written to.
    pub runs: Option<PathBuf>,
    /// The calibration file.
    pub calibration: Option<PathBuf>,
}

/// Whether an `ended` event by `by` ends a `wait`: the performance is over
/// and nothing follows it.
fn is_terminal(by: EndedBy, next: &Option<String>) -> bool {
    match by {
        EndedBy::Stop | EndedBy::Enough => true,
        EndedBy::End => next.is_none(),
        _ => false,
    }
}

/// One block of the loaded score and the mixer frame its timeline begins
/// at. A link's index in `App::chain` is its mixer segment number.
struct Link {
    block: usize,
    /// The block as this link plays it: without its count-in when entered
    /// attacca. `tl` is compiled from it and its performance judges it.
    ex: Exercise,
    tl: Arc<Timeline>,
    origin: u64,
    /// The beats of the links before and after it in the score.
    neighbours: Neighbours,
}

/// The performance of a block of the playing score, open from its start
/// (a block entered attacca: from the start of the boundary region before
/// it) until it is stopped, replaced or its judged span has ended.
struct Live {
    block: usize,
    perf: Performance,
    /// The voice and tone each of the block's keys plays.
    keys: HashMap<Trigger, (usize, char)>,
    /// The mixer frame the block's timeline begins at.
    origin: u64,
    /// The latest snapshot of this run that read `playing`; strokes after
    /// a finite block's end are judged from it.
    last_playing: Option<ClockSnapshot>,
}

/// The status line's calibration text: `calibration: +12 ms`, else
/// `uncalibrated`; `notice` (a failed calibration run or an unreadable file)
/// takes precedence.
pub fn calibration_text(calibration: Option<&Calibration>, notice: Option<&str>) -> String {
    match (notice, calibration) {
        (Some(n), _) => n.to_string(),
        (None, Some(c)) => format!("calibration: {:+.0} ms", c.offset_ms),
        (None, None) => "uncalibrated".into(),
    }
}

/// Where the scroll area moves on the next frame.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Scroll {
    Top,
    /// Align the block's top with the top of the view.
    Block(usize),
    /// Scroll just enough to bring the block fully into view.
    Reveal(usize),
}

/// The window's state. `S` is the engine's stream; tests use an engine with
/// none.
pub struct App<S = cpal::Stream> {
    requests: mpsc::Receiver<Pending>,
    engine: Engine<S>,
    bank: Bank,
    socket: PathBuf,
    soundfont: PathBuf,
    doc: Option<Doc>,
    /// The score loaded into the engine, head first.
    chain: Vec<Link>,
    /// The last link of `chain` sent to the engine.
    queued: usize,
    /// The block whose score is loaded and waits for the user to start it.
    armed: Option<usize>,
    cache: CommonMarkCache,
    scroll: Option<Scroll>,
    /// The last failure from a button or key, shown in the status line.
    notice: Option<String>,
    /// Mapped strokes from the input hub.
    hits: mpsc::Receiver<(Hit, Part)>,
    /// Focus, arming and device status shared with the input hub.
    input: Arc<Shared>,
    store: Store,
    /// The open performances of the playing score, in score order: two
    /// across the boundary region between links, three on a link no longer
    /// than that region.
    live: Vec<Live>,
    /// `Engine::mark` after the commands that loaded the score.
    mark: u64,
    /// The last link of `chain` whose performance has been opened, if its
    /// block has a part.
    opened: usize,
    /// The performances closed since a score with a part last started, one
    /// per block, still drawn on their blocks.
    closed: Vec<(usize, Performance)>,
    /// The run of the last performance closed in this session.
    last_run: Option<Run>,
    /// The file `last_run` was written to, if it was; cleared when a
    /// performance opens.
    written: Option<PathBuf>,
    /// The offset subtracted from every stroke outside the calibration
    /// block; `None` when uncalibrated.
    calibration: Option<Calibration>,
    /// Why the calibration is not what the file says, for the status line.
    calibration_notice: Option<String>,
    /// The block whose `started` was emitted, still owed its `ended`, when
    /// it has no performance to emit it on close.
    playing_block: Option<usize>,
    /// The `wait`s listening for transport events.
    subscribers: Vec<mpsc::Sender<Event>>,
    /// The last transport event emitted.
    last_event: Option<Event>,
}

impl<S> App<S> {
    pub fn new(
        requests: mpsc::Receiver<Pending>,
        engine: Engine<S>,
        bank: Bank,
        socket: PathBuf,
        soundfont: PathBuf,
        hits: mpsc::Receiver<(Hit, Part)>,
        input: Arc<Shared>,
    ) -> App<S> {
        App {
            requests,
            engine,
            bank,
            socket,
            soundfont,
            doc: None,
            chain: Vec::new(),
            queued: 0,
            armed: None,
            cache: CommonMarkCache::default(),
            scroll: None,
            notice: None,
            hits,
            input,
            store: Store::default(),
            live: Vec::new(),
            mark: 0,
            opened: 0,
            closed: Vec::new(),
            last_run: None,
            written: None,
            calibration: None,
            calibration_notice: None,
            playing_block: None,
            subscribers: Vec::new(),
            last_event: None,
        }
    }

    /// Keeps runs and the calibration in `store`, loading the calibration
    /// from it.
    pub fn with_store(mut self, store: Store) -> App<S> {
        (self.calibration, self.calibration_notice) = match store.calibration.as_deref() {
            None => (None, None),
            Some(path) => match Calibration::load(path) {
                Ok(c) => (c, None),
                Err(e) => (
                    None,
                    Some(format!("uncalibrated: {} unreadable: {e}", path.display())),
                ),
            },
        };
        self.store = store;
        self
    }

    /// Judges the strokes the hub forwarded and closes the windows that
    /// have passed by `now`. First opens the performance of the next link
    /// of the score once its judged span has begun. Strokes are judged
    /// against the latest snapshot of the score while it plays, and against
    /// the last playing one once a finite block has finished; otherwise (no
    /// performance, not yet started, paused, stopped) they are dropped. Each
    /// stroke goes to one open performance whose block binds its key, as
    /// `route` picks it. A performance closes once the heard clock passes
    /// its judged span. A calibration with enough hits is
    /// stopped and closed once the clicks those hits matched have sounded;
    /// if the stop cannot be sent, the next call tries again. A dead stream
    /// ends the play `by=stop`: every open performance and the playing
    /// block without one.
    fn judge_hits(&mut self, now: Mono) {
        let hits: Vec<(Hit, Part)> = self.hits.try_iter().collect();
        let rate = self.engine.sample_rate();
        if self.engine.failure().is_some() {
            // The clock froze when the stream died; extrapolating it would
            // turn every later target into a miss that never sounded.
            self.close_all(EndedBy::Stop);
            return;
        }
        if self.live.is_empty() && self.opened + 1 >= self.chain.len() {
            return;
        }
        self.open_ahead(now);
        let snap = self.engine.since(self.mark);
        let mut refs = Vec::with_capacity(self.live.len());
        for live in &mut self.live {
            let finished = snap.is_some_and(|s| engine::finished(&s, 0, live.perf.end()));
            let reference = match snap {
                Some(s) if s.playing => {
                    live.last_playing = Some(s);
                    Some(s)
                }
                Some(_) if finished => live.last_playing,
                _ => None,
            };
            refs.push((reference, finished));
        }
        for (hit, _) in &hits {
            self.route(hit, &refs);
        }
        let mut ended = Vec::new();
        for (i, (live, (reference, finished))) in self.live.iter_mut().zip(&refs).enumerate() {
            match reference {
                Some(r) => {
                    let now = heard(r, rate, now);
                    live.perf.advance(now);
                    if live.perf.closed(now) {
                        ended.push(i);
                    }
                }
                None if *finished => ended.push(i),
                // TODO(correctness-judged-block-never-ends-when-score-ends-inside-its-trail)
                None => {}
            }
        }
        for (n, i) in ended.iter().enumerate() {
            let live = self.live.remove(i - n);
            let event = self.retire(live, EndedBy::End);
            self.emit(event);
        }
        if !ended.is_empty() {
            self.rearm();
        }
        let enough = self.live.iter().any(|l| {
            l.perf.is_calibration()
                && l.perf.hit_count() >= perf::CALIBRATION_TAPS
                && l.perf.targets_heard()
        });
        if enough && self.engine.stop().is_ok() {
            self.close_all(EndedBy::Enough);
        }
    }

    /// Judges `hit` in the open performance it belongs to: among those
    /// whose block binds its key and that have a reference snapshot in
    /// `refs`, the one with the nearest unclaimed target within its window,
    /// else the one with the nearest existing target at any distance, else
    /// the last whose block has begun by the stroke's judged frame, else
    /// the first.
    fn route(&mut self, hit: &Hit, refs: &[(Option<ClockSnapshot>, bool)]) {
        let rate = self.engine.sample_rate();
        let eligible: Vec<(usize, f64, usize, char)> = self
            .live
            .iter()
            .zip(refs)
            .enumerate()
            .filter_map(|(i, (live, (reference, _)))| {
                let r = reference.as_ref()?;
                let &(voice, tone) = live.keys.get(&hit.trigger)?;
                Some((i, heard(r, rate, hit.at), voice, tone))
            })
            .collect();
        let least = |distance: &dyn Fn(&Performance, f64, usize) -> Option<f64>| {
            eligible
                .iter()
                .filter_map(|&(i, at, voice, _)| {
                    Some((distance(&self.live[i].perf, at, voice)?, i))
                })
                .min_by(|a, b| a.0.total_cmp(&b.0))
                .map(|(_, i)| i)
        };
        let nearest = least(&Performance::candidate).or_else(|| least(&Performance::nearest));
        let spanning = || {
            eligible
                .iter()
                .rev()
                .find(|&&(i, at, ..)| {
                    let live = &self.live[i];
                    live.origin as f64 <= live.perf.judged_frame(at)
                })
                .or(eligible.first())
                .map(|e| e.0)
        };
        let Some(chosen) = nearest.or_else(spanning) else {
            return;
        };
        let &(_, at, voice, tone) = eligible
            .iter()
            .find(|e| e.0 == chosen)
            .expect("the chosen performance is eligible");
        self.live[chosen].perf.judge(at, voice, tone, hit.velocity);
    }

    /// The calibration offset strokes on the shown lesson are judged with:
    /// 0 on the calibration lesson, which measures one, and uncalibrated.
    fn offset_ms(&self) -> f64 {
        match (&self.calibration, &self.doc) {
            (Some(c), Some(doc)) if doc.path != perf::CALIBRATION_NAME => c.offset_ms,
            _ => 0.0,
        }
    }

    /// Opens the performance of each later link of the score whose judged
    /// span has begun on the clock strokes are judged on (the heard clock
    /// less the calibration offset), so a stroke drained in this pass in
    /// the boundary region before it finds it open.
    fn open_ahead(&mut self, now: Mono) {
        let Some(snap) = self.engine.since(self.mark).filter(|s| s.playing) else {
            return;
        };
        let rate = self.engine.sample_rate();
        let judged = heard(&snap, rate, now) - self.offset_ms() * rate as f64 / 1000.0;
        while let Some(next) = self.chain.get(self.opened + 1) {
            let window = perf::window_frames(next.ex.window_ms(), rate);
            if judged < perf::span_start(next.origin, window, next.neighbours.before) {
                break;
            }
            self.opened += 1;
            self.open_link(self.opened);
            self.rearm();
        }
    }

    /// Opens the performance of the score just started from `block`'s
    /// count-in, the previous play already closed. A score with a part
    /// drops the performances closed before it.
    fn open(&mut self, block: usize) {
        if self.chain.iter().any(|l| l.ex.has_part()) {
            self.closed.clear();
        }
        self.open_link(0);
        self.started(block);
    }

    /// Opens a performance of link `i` of the score if its block has a
    /// player voice. A link after the head was entered attacca: its run
    /// records no count-in.
    fn open_link(&mut self, i: usize) {
        let (Some(doc), Some(link)) = (self.doc.as_ref(), self.chain.get(i)) else {
            return;
        };
        if !link.ex.has_part() {
            return;
        }
        let input = self.input.status().summary();
        let latency_ms = self
            .engine
            .audio_status()
            .map(|(_, latency)| latency.as_secs_f64() * 1000.0);
        let perf = Performance::new(
            &link.ex,
            &link.tl,
            link.origin,
            link.neighbours,
            self.engine.sample_rate(),
            perf::Conditions {
                lesson: &doc.path,
                input: &input,
                offset_ms: self.offset_ms(),
                latency_ms,
            },
        );
        let live = Live {
            block: link.block,
            perf,
            keys: part_keys(&link.ex),
            origin: link.origin,
            last_playing: None,
        };
        self.written = None;
        self.live.push(live);
    }

    /// Closes every open performance, in score order, and ends the playing
    /// block without one; emits their `ended` events, only the last of
    /// which can be terminal.
    fn close_all(&mut self, ended_by: EndedBy) {
        let mut events: Vec<Event> = std::mem::take(&mut self.live)
            .into_iter()
            .map(|live| self.retire(live, ended_by))
            .collect();
        events.extend(self.end_block(ended_by, None));
        let last = events.len().saturating_sub(1);
        for (i, mut event) in events.into_iter().enumerate() {
            event.terminal &= i == last;
            self.emit(event);
        }
    }

    /// Closes `live`: `finish`es it when the block ended, keeps its run as
    /// the last one, writes the run file when it has a hit or a miss, and
    /// takes a new offset from a calibration block however it closed.
    /// Returns its `ended` event.
    fn retire(&mut self, mut live: Live, ended_by: EndedBy) -> Event {
        if ended_by == EndedBy::End {
            live.perf.finish();
        }
        let run = live.perf.run(ended_by);
        if live.perf.is_calibration() {
            self.calibrate(&run, ended_by);
        }
        self.written = None;
        if !run.hits.is_empty() || !run.misses.is_empty() {
            let written = match self.store.runs.as_deref() {
                Some(dir) => run
                    .write(dir)
                    .map_err(|e| format!("{e}: {}", dir.display())),
                None => Err("no home directory".into()),
            };
            match written {
                Ok(path) => self.written = Some(path),
                Err(e) => self.notice = Some(format!("run not saved: {e}")),
            }
        }
        let tally = live.perf.tally();
        let next = match ended_by {
            EndedBy::End => self.successor(live.block),
            _ => None,
        };
        let event = Event {
            by: Some(ended_by),
            terminal: is_terminal(ended_by, &next),
            next,
            cycles_played: Some(run.cycles_played),
            cycles: run.cycles,
            hit: Some(tally.hit),
            miss: Some(tally.miss),
            extra: Some(tally.extra),
            run: self.written.as_ref().map(|p| p.display().to_string()),
            ..Event::new(EventKind::Ended, Some(run.label.clone()))
        };
        self.last_run = Some(run);
        self.closed.retain(|(b, _)| *b != live.block);
        self.closed.push((live.block, live.perf));
        event
    }

    /// The label of the block the score went on into after `block`: the
    /// next link of the chain, once the mixer has swapped to it.
    fn successor(&self, block: usize) -> Option<String> {
        let i = self.chain.iter().position(|l| l.block == block)?;
        let next = self.chain.get(i + 1)?;
        let swapped = self
            .engine
            .current()
            .is_some_and(|s| s.segment as usize > i);
        swapped.then(|| self.label_of(next.block)).flatten()
    }

    fn label_of(&self, block: usize) -> Option<String> {
        Some(self.doc.as_ref()?.block(block)?.label.clone())
    }

    /// Sends `event` to every `wait`, dropping those whose client is gone,
    /// and keeps it as the last event.
    fn emit(&mut self, event: Event) {
        self.subscribers.retain(|s| s.send(event.clone()).is_ok());
        self.last_event = Some(event);
    }

    /// Emits `started` for `block`; a block without a part is owed its
    /// `ended` by `end_block`, one with a part gets it when its performance
    /// closes.
    fn started(&mut self, block: usize) {
        let has_part = self
            .doc
            .as_ref()
            .and_then(|d| d.block(block))
            .is_some_and(Exercise::has_part);
        self.playing_block = (!has_part).then_some(block);
        self.emit(Event::new(EventKind::Started, self.label_of(block)));
    }

    /// The `ended` event of the playing block without a part, if there is
    /// one; it is no longer owed.
    fn end_block(&mut self, by: EndedBy, next: Option<String>) -> Option<Event> {
        let block = self.playing_block.take()?;
        let cycles = self.doc.as_ref()?.block(block)?.cycles;
        let played = match by {
            EndedBy::End => cycles,
            _ => Some(self.cycles_heard(block)),
        };
        Some(Event {
            by: Some(by),
            terminal: is_terminal(by, &next),
            next,
            cycles_played: played,
            cycles,
            ..Event::new(EventKind::Ended, self.label_of(block))
        })
    }

    /// The whole cycles of `block`'s link the mixer has played.
    fn cycles_heard(&self, block: usize) -> u32 {
        let (Some(snap), Some(link)) = (
            self.engine.current(),
            self.chain.iter().find(|l| l.block == block),
        ) else {
            return 0;
        };
        let tl = &link.tl;
        let into = snap.frame.saturating_sub(link.origin + tl.count_in_len);
        let played = into.checked_div(tl.cycle_len).unwrap_or(0) as u32;
        tl.cycles.map_or(played, |c| played.min(c))
    }

    /// What a `wait` is told first: the last event if it was terminal (the
    /// performance it waits for is over), else the transport's state.
    fn state_event(&self) -> Event {
        if let Some(e) = self.last_event.as_ref().filter(|e| e.terminal) {
            return e.clone();
        }
        let Some(doc) = self.doc.as_ref() else {
            return Event::new(EventKind::Idle, None);
        };
        if let Some(b) = self.armed {
            return Event::new(EventKind::Armed, self.label_of(b));
        }
        let label = doc.active.and_then(|b| self.label_of(b));
        let cycles = doc.active.and_then(|b| doc.block(b)).and_then(|e| e.cycles);
        match self.transport_at(Mono::now()) {
            Transport::Playing(p) => Event {
                cycle: Some(p.cycle + 1),
                cycles,
                ..Event::new(EventKind::Playing, label)
            },
            Transport::CountIn => Event::new(EventKind::CountIn, label),
            Transport::Paused => Event::new(EventKind::Paused, label),
            Transport::Armed | Transport::Stopped => Event::new(EventKind::Idle, None),
        }
    }

    /// Answers a `wait`: sends `events` the state event, and every later
    /// event unless the state event already ends the wait.
    fn wait(&mut self, events: mpsc::Sender<Event>) -> Response {
        let state = self.state_event();
        if events.send(state.clone()).is_ok() && !state.terminal {
            self.subscribers.push(events);
        }
        Response {
            ok: true,
            ..Response::default()
        }
    }

    /// Arms the hub with the parts of every block with an open performance
    /// and of the active block; a key bound in several plays the sounding
    /// (active) block's part, else the later block's.
    fn rearm(&mut self) {
        let Some(doc) = self.doc.as_ref() else {
            return;
        };
        let mut blocks: Vec<usize> = self
            .live
            .iter()
            .map(|l| l.block)
            .filter(|&b| Some(b) != doc.active)
            .collect();
        let order = |b: &usize| self.chain.iter().position(|l| l.block == *b);
        blocks.sort_by_key(order);
        blocks.dedup();
        blocks.extend(doc.active);
        match arming(blocks.iter().filter_map(|&b| doc.block(b)), &mut self.bank) {
            Ok(a) => self.input.set_arming(a),
            Err(e) => self.notice = Some(format!("{e:#}")),
        }
    }
    /// Takes the offset a calibration run measured, saving it. A run with
    /// too few taps changes nothing, except that a deliberate stop says
    /// why on the status line in place of the offset still in effect.
    fn calibrate(&mut self, run: &Run, ended_by: EndedBy) {
        match Calibration::from_run(run) {
            Ok(c) => {
                self.calibration_notice = match self.store.calibration.as_deref() {
                    Some(path) => c.save(path).err().map(|e| {
                        format!(
                            "calibration: {:+.0} ms, not saved: {e}: {}",
                            c.offset_ms,
                            path.display()
                        )
                    }),
                    None => Some(format!(
                        "calibration: {:+.0} ms, not saved: no home directory",
                        c.offset_ms
                    )),
                };
                self.calibration = Some(c);
            }
            Err(taps) if ended_by == EndedBy::Stop => {
                self.calibration_notice = Some(format!(
                    "calibration: not enough taps ({taps}/{})",
                    perf::MIN_CALIBRATION_TAPS
                ));
            }
            Err(_) => {}
        }
    }

    /// Closes the open performances as the window exits.
    fn quit(&mut self) {
        self.close_all(EndedBy::Quit);
    }

    /// Whether a performance is open on a score the mixer has finished:
    /// the stretch after its end in which late strokes still count.
    fn post_end(&self) -> bool {
        self.engine.since(self.mark).is_some_and(|s| {
            self.live
                .iter()
                .any(|live| engine::finished(&s, 0, live.perf.end()))
        })
    }

    /// Acts on every live request and answers it.
    fn serve_requests(&mut self) {
        for p in drain(&self.requests) {
            let response = match (p.request, p.events) {
                (Request::Wait, Some(events)) => self.wait(events),
                (request, _) => self.handle(request),
            };
            let _ = p.reply.send(response);
        }
    }

    fn handle(&mut self, request: Request) -> Response {
        let shows = matches!(request, Request::Show { .. } | Request::Calibrate);
        let result = match request {
            Request::Show { path, lesson, play } => self.show(path, lesson, play.as_deref()),
            Request::Play { label } => self.play_label(label.as_deref()),
            Request::Stop => self.stop(),
            Request::Report => return self.report(),
            Request::Calibrate => self.show_calibration(),
            Request::Wait => Err("wait needs an event stream".into()),
        };
        match result {
            Ok(()) => Response {
                warnings: if shows {
                    self.keyboard_warning().into_iter().collect()
                } else {
                    Vec::new()
                },
                armed: self.armed_label(),
                ..Response::ok(self.labels())
            },
            Err(e) => Response::err(e),
        }
    }

    fn report(&self) -> Response {
        self.report_at(Mono::now())
    }

    /// The run so far of the open performance of the block heard at `now`,
    /// else of the last one opened; else the last one closed in this
    /// session with the file it was written to.
    fn report_at(&self, now: Mono) -> Response {
        let sounding = self.link_now(now).map(|l| l.block);
        let open = self
            .live
            .iter()
            .find(|l| Some(l.block) == sounding)
            .or(self.live.last());
        match (open, &self.last_run) {
            (Some(live), _) => Response::run(live.perf.run(EndedBy::Open), None),
            (None, Some(run)) => Response::run(
                run.clone(),
                self.written.as_ref().map(|p| p.display().to_string()),
            ),
            (None, None) => Response::err("no performance yet"),
        }
    }

    /// Shows the built-in calibration lesson and plays its block.
    fn show_calibration(&mut self) -> Result<(), String> {
        let mut diag = Diagnostics::default();
        let lesson = lesson::parse(perf::CALIBRATION_LESSON, Path::new("."), &mut diag)
            .ok_or_else(|| {
                format!(
                    "the calibration lesson is invalid: {}",
                    diag.errors.join("; ")
                )
            })?;
        self.show(perf::CALIBRATION_NAME.into(), lesson, Some("1"))
    }

    /// The warning `show` returns when the shown lesson has a player part
    /// but no keyboard is readable.
    fn keyboard_warning(&self) -> Option<String> {
        let doc = self.doc.as_ref()?;
        let players = doc.lesson.blocks().any(Exercise::has_part);
        let readable = matches!(self.input.status(), input::InputStatus::Devices(n) if n >= 1);
        (players && !readable).then(|| {
            "no keyboard device is readable; parts with \"keys\" will record only misses — see \
             README \"Keyboard access\""
                .to_string()
        })
    }

    /// The label of the armed block, if one is armed.
    fn armed_label(&self) -> Option<String> {
        let doc = self.doc.as_ref()?;
        Some(doc.block(self.armed?)?.label.clone())
    }

    /// Whether starting the score from `block` arms it rather than playing
    /// it: some block of it gives the user a part, and the lesson is not the
    /// calibration lesson, whose clicks start at once.
    fn arms(doc: &Doc, block: usize) -> bool {
        doc.path != perf::CALIBRATION_NAME && score_has_part(&doc.lesson, block)
    }

    fn labels(&self) -> Vec<String> {
        self.doc
            .as_ref()
            .map(|d| d.lesson.labels())
            .unwrap_or_default()
    }

    /// Stops playback, replaces the document and scrolls to the top; then
    /// starts the score from block `play` if given, or arms it when it has a
    /// part (see `arms`). Everything that can fail
    /// — the label, the score's timelines, room in the engine's queue — is
    /// checked before anything changes, so a failed `show` leaves the window
    /// as it was.
    fn show(&mut self, path: String, lesson: Lesson, play: Option<&str>) -> Result<(), String> {
        let doc = Doc::new(path, lesson);
        let start = match play {
            Some(label) => {
                let block = pick_block(Some(&doc.lesson), Some(label))?;
                let rate = self.engine.sample_rate();
                let score = compile_score(&mut self.bank, rate, &doc, block)
                    .map_err(|e| format!("{e:#}"))?;
                Some((block, score))
            }
            None => None,
        };
        let arm = start.as_ref().is_some_and(|(b, _)| Self::arms(&doc, *b));
        let stopping = self.active_block().is_some();
        let sends =
            usize::from(stopping) + start.as_ref().map_or(0, |(_, s)| s.sends()) - usize::from(arm);
        self.engine.reserve(sends).map_err(|e| format!("{e:#}"))?;
        if stopping {
            self.engine.stop().map_err(|e| format!("{e:#}"))?;
        }
        self.notice = None;
        self.close_all(EndedBy::Show);
        self.closed.clear();
        self.chain.clear();
        self.doc = Some(doc);
        self.scroll = Some(Scroll::Top);
        self.input.set_arming(None);
        self.armed = None;
        let shown = Event {
            lesson: self.doc.as_ref().map(|d| d.path.clone()),
            ..Event::new(EventKind::Shown, None)
        };
        self.emit(shown);
        if let Some((block, score)) = start {
            // Room for every send was reserved above.
            self.load(score).map_err(|e| format!("{e:#}"))?;
            if arm {
                self.armed = Some(block);
                self.emit(Event::new(EventKind::Armed, self.label_of(block)));
            } else {
                self.engine.play().map_err(|e| format!("{e:#}"))?;
                self.open(block);
            }
            self.scroll = Some(Scroll::Block(block));
        }
        Ok(())
    }

    /// Starts the score from the labelled block, or arms it when it has a
    /// part (see `arms`); scrolls to it.
    fn play_label(&mut self, label: Option<&str>) -> Result<(), String> {
        let doc = self.doc.as_ref();
        let block = pick_block(doc.map(|d| &d.lesson), label)?;
        let r = if doc.is_some_and(|d| Self::arms(d, block)) {
            self.arm(block)
        } else {
            self.start(block)
        };
        r.map_err(|e| format!("{e:#}"))?;
        self.scroll = Some(Scroll::Block(block));
        Ok(())
    }

    /// Stops whatever plays, closing its performances, and loads the score
    /// from `block` without playing it; the hub is armed with its head's
    /// parts, so they sound, but nothing is judged until `start`. Changes
    /// nothing if it cannot.
    fn arm(&mut self, block: usize) -> Result<()> {
        let doc = self.doc.as_ref().context("no lesson loaded")?;
        let score = compile_score(&mut self.bank, self.engine.sample_rate(), doc, block)?;
        // `load` sends every command of the score but its `Play`.
        self.engine.reserve(score.sends() - 1)?;
        self.close_all(EndedBy::Stop);
        self.load(score)?;
        self.armed = Some(block);
        self.emit(Event::new(EventKind::Armed, self.label_of(block)));
        Ok(())
    }

    /// Stops the active block and closes its performance.
    fn stop(&mut self) -> Result<(), String> {
        self.stop_active().map_err(|e| format!("{e:#}"))
    }

    fn stop_active(&mut self) -> Result<()> {
        if self.active_block().is_some() {
            self.engine.stop()?;
        }
        self.armed = None;
        self.close_all(EndedBy::Stop);
        Ok(())
    }

    /// Loads `score`: its head into the engine and its second link queued
    /// after it, each with its block's mutes; arms the hub with the head's
    /// parts and makes the head the active and selected block. Snapshots
    /// are read from the loaded score's clock on. Sends `score.sends() - 1`
    /// commands.
    fn load(&mut self, score: Score) -> Result<()> {
        let doc = self.doc.as_mut().context("no lesson loaded")?;
        let head = score.chain.first().context("an empty score")?;
        self.engine.load(head.tl.clone())?;
        if let Some(second) = score.chain.get(1) {
            self.engine
                .queue(second.tl.clone(), 0, doc.blocks[second.block].muted)?;
        }
        self.input.set_arming(score.arming);
        doc.active = Some(head.block);
        doc.selected = head.block;
        self.queued = score.chain.len().min(2) - 1;
        self.opened = 0;
        self.chain = score.chain;
        self.mark = self.engine.mark();
        Ok(())
    }

    /// Plays the score from `block`, from its count-in, and opens its
    /// performance; whatever was playing ends `by=restart` when it was
    /// `block`, else `by=switch`. Changes nothing if it cannot.
    fn start(&mut self, block: usize) -> Result<()> {
        let doc = self.doc.as_ref().context("no lesson loaded")?;
        let score = compile_score(&mut self.bank, self.engine.sample_rate(), doc, block)?;
        self.engine.reserve(score.sends())?;
        let same = self.live.iter().any(|l| l.block == block) || self.playing_block == Some(block);
        self.close_all(if same {
            EndedBy::Restart
        } else {
            EndedBy::Switch
        });
        self.load(score)?;
        self.engine.play()?;
        self.armed = None;
        self.open(block);
        Ok(())
    }

    /// Plays `block`: continues it if it is the paused active block, and its
    /// performance with it; otherwise starts it from its count-in. Changes
    /// nothing if it cannot.
    fn resume(&mut self, block: usize) -> Result<()> {
        if self.active_block() == Some(block) && self.paused_now() == Some(true) {
            self.engine.play()?;
            if let Some(doc) = self.doc.as_mut() {
                doc.selected = block;
            }
            return Ok(());
        }
        self.start(block)
    }

    fn active_block(&self) -> Option<usize> {
        self.doc.as_ref()?.active
    }

    /// The link being heard now while the score plays, else the one it
    /// stopped or paused in; `None` before the callback applied the latest
    /// `Load`.
    fn link_now(&self, now: Mono) -> Option<&Link> {
        let snap = self.engine.current()?;
        let at = if snap.playing {
            heard(&snap, self.engine.sample_rate(), now)
        } else {
            snap.frame as f64
        };
        self.chain.get(link_at(&self.chain, at)?)
    }

    fn position_at(&self, now: Mono) -> Option<engine::Position> {
        let link = self.link_now(now)?;
        self.engine.position_at(&link.tl, link.origin, now)
    }

    fn count_in_beat_at(&self, now: Mono) -> Option<u32> {
        let link = self.link_now(now)?;
        self.engine.count_in_beat_at(&link.tl, link.origin, now)
    }

    #[cfg(test)]
    fn count_in_beat_now(&self) -> Option<u32> {
        self.count_in_beat_at(Mono::now())
    }

    fn paused_now(&self) -> Option<bool> {
        let link = self.link_now(Mono::now())?;
        self.engine.paused_now(&link.tl, link.origin)
    }

    /// Drops the timelines the engine has let go of; keeps the mixer one
    /// link ahead of the score, and the window on the link being heard:
    /// once the mixer has swapped to the last link queued, queues the one
    /// after it with its block's current mutes; once the heard clock passes
    /// a link's origin, makes that link's block active and selected, scrolls
    /// to it and rearms the hub.
    fn follow_score(&mut self, now: Mono) {
        // A swap returns a timeline without a command to drain it.
        self.engine.drain_returned();
        let Some(snap) = self.engine.current() else {
            return;
        };
        let Some(doc) = self.doc.as_mut() else {
            return;
        };
        let segment = snap.segment as usize;
        if segment == self.queued
            && let Some(next) = self.chain.get(segment + 1)
        {
            let muted = doc.blocks[next.block].muted;
            match self.engine.queue(next.tl.clone(), snap.segment, muted) {
                Ok(()) => self.queued = segment + 1,
                Err(e) => self.notice = Some(format!("{e:#}")),
            }
        }
        if !snap.playing {
            let ended = self.playing_block.is_some_and(|b| {
                self.chain
                    .iter()
                    .find(|l| l.block == b)
                    .is_some_and(|l| engine::finished(&snap, l.origin, l.tl.end))
            });
            if ended && let Some(event) = self.end_block(EndedBy::End, None) {
                self.emit(event);
            }
            return;
        }
        let at = heard(&snap, self.engine.sample_rate(), now);
        let Some(block) = link_at(&self.chain, at).map(|i| self.chain[i].block) else {
            return;
        };
        if doc.active == Some(block) {
            return;
        }
        doc.active = Some(block);
        doc.selected = block;
        self.scroll = Some(Scroll::Block(block));
        self.rearm();
        if let Some(event) = self.end_block(EndedBy::End, self.label_of(block)) {
            self.emit(event);
        }
        self.started(block);
    }

    /// Whether the active block is sounding (count-in included).
    fn playing(&self) -> bool {
        self.active_block().is_some() && self.engine.current().is_some_and(|s| s.playing)
    }

    /// Whether the window must keep redrawing: something is sounding, a
    /// command the clock does not reflect yet may start it, or a finished
    /// block's judged span is still open.
    fn animating(&self) -> bool {
        self.playing() || self.engine.commands_pending() || self.post_end()
    }

    #[cfg(test)]
    fn transport(&self) -> Transport {
        self.transport_at(Mono::now())
    }

    /// The status line's transport text at `now`, naming the active block.
    fn transport_line(&self, doc: &Doc, now: Mono) -> String {
        let ex = doc.active.and_then(|b| doc.block(b));
        transport_text(
            ex.map(|e| e.label.as_str()),
            ex.and_then(|e| e.cycles),
            self.transport_at(now),
        )
    }

    fn transport_at(&self, now: Mono) -> Transport {
        if self.active_block().is_none() {
            return Transport::Stopped;
        }
        if self.armed.is_some() {
            return Transport::Armed;
        }
        let Some(snap) = self.engine.current() else {
            return Transport::Stopped;
        };
        if snap.playing {
            return match self.position_at(now) {
                Some(p) => Transport::Playing(p),
                None => Transport::CountIn,
            };
        }
        if self.paused_now() == Some(true) {
            Transport::Paused
        } else {
            Transport::Stopped
        }
    }

    /// Flips voice `voice` of `block`; nothing if the block has no such
    /// voice.
    fn toggle_mute(&mut self, block: usize, voice: usize) -> Result<()> {
        let Some(doc) = self.doc.as_mut() else {
            return Ok(());
        };
        let voices = doc.block(block).map_or(0, |ex| ex.voices.len());
        if voice >= voices {
            return Ok(());
        }
        let segment = self.chain.iter().position(|l| l.block == block);
        if segment.is_some() {
            self.engine.reserve(1)?;
        }
        let m = &mut doc.blocks[block].muted[voice];
        *m = !*m;
        let mute = *m;
        if let Some(segment) = segment {
            self.engine.set_mute(voice, mute, segment as u32)?;
        }
        Ok(())
    }

    fn note(&mut self, r: Result<()>) {
        if let Err(e) = r {
            self.notice = Some(format!("{e:#}"));
        }
    }

    /// `↓`/`j` and `↑`/`k` move the selection without playing, `space`
    /// play/pause, `r` restart, `h` hide/reveal, `1`–`9` mute: all on the
    /// selected block. `q` quits. Every key acted on is consumed, so no
    /// focused widget sees it too.
    fn keys(&mut self, ctx: &egui::Context) {
        let none = egui::Modifiers::NONE;
        let q = ctx.input_mut(|i| i.consume_key(none, Key::Q));
        if q {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
        let Some(doc) = self.doc.as_ref() else {
            return;
        };
        let blocks = doc.blocks.len();
        if blocks == 0 {
            return;
        }
        let (down, up, space, r, h, digit) = ctx.input_mut(|i| {
            let mut count = |keys: [Key; 2]| {
                keys.iter()
                    .map(|k| i.count_and_consume_key(none, *k))
                    .sum::<usize>()
            };
            let down = count([Key::ArrowDown, Key::J]);
            let up = count([Key::ArrowUp, Key::K]);
            let space = i.consume_key(none, Key::Space);
            let r = i.consume_key(none, Key::R);
            let h = i.consume_key(none, Key::H);
            let digit = DIGITS.iter().position(|k| i.consume_key(none, *k));
            (down, up, space, r, h, digit)
        });
        if down + up > 0 {
            let doc = self.doc.as_mut().expect("checked above");
            let moved = (doc.selected + down).saturating_sub(up).min(blocks - 1);
            doc.selected = moved;
            self.scroll = Some(Scroll::Reveal(moved));
        }
        let block = self.doc.as_ref().expect("checked above").selected;
        if space {
            let r = if self.active_block() == Some(block) && self.playing() {
                self.engine.pause()
            } else {
                self.resume(block)
            };
            self.note(r);
        }
        if r {
            let r = self.start(block);
            self.note(r);
        }
        if h {
            let doc = self.doc.as_mut().expect("checked above");
            let hidden = &mut doc.blocks[block].hidden;
            *hidden = !*hidden;
        }
        if let Some(v) = digit {
            let r = self.toggle_mute(block, v);
            self.note(r);
        }
    }

    /// What the status line reports in red: a dead stream, else the last
    /// failure from a button or key.
    fn problem(&self) -> Option<String> {
        match self.engine.failure() {
            Some(f) => Some(format!("{f}; restart `djembot gui`")),
            None => self.notice.clone(),
        }
    }

    fn calibration_status(&self) -> String {
        calibration_text(
            self.calibration.as_ref(),
            self.calibration_notice.as_deref(),
        )
    }

    fn audio_status(&self) -> String {
        audio_text(self.engine.audio_status())
    }

    fn status_line(&self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            let Some(doc) = &self.doc else {
                ui.label(format!(
                    "listening on {} · soundfont {}",
                    self.socket.display(),
                    self.soundfont.display()
                ));
                ui.separator();
                ui.label(self.input.status().status_text());
                ui.separator();
                ui.label(self.audio_status());
                ui.separator();
                ui.label(self.calibration_status());
                return;
            };
            let text = self.transport_line(doc, Mono::now());
            ui.label(&doc.path);
            ui.separator();
            ui.label(RichText::new(text).strong());
            ui.separator();
            ui.label(format!("soundfont {}", self.soundfont.display()));
            ui.separator();
            ui.label(self.input.status().status_text());
            ui.separator();
            ui.label(self.audio_status());
            ui.separator();
            ui.label(self.calibration_status());
            if let Some(path) = &self.written {
                ui.separator();
                ui.label(format!("run: {}", path.display()));
            }
            if let Some(problem) = self.problem() {
                ui.separator();
                ui.colored_label(Color32::RED, problem);
            }
        });
    }

    fn empty_view(&self, ui: &mut egui::Ui) {
        ui.vertical_centered(|ui| {
            ui.add_space(40.0);
            ui.heading("Djembot");
            ui.add_space(8.0);
            ui.label(format!("Listening on {}", self.socket.display()));
            ui.add_space(8.0);
            ui.label("Show a lesson with:");
            ui.code("djembot show lessons/000-twelve-vs-sixteen.md");
        });
    }

    fn lesson_view(&mut self, ui: &mut egui::Ui) {
        let now = Mono::now();
        let scroll = self.scroll.take();
        let mut area = egui::ScrollArea::vertical().auto_shrink([false, false]);
        if scroll == Some(Scroll::Top) {
            area = area.vertical_scroll_offset(0.0);
        }
        let mut actions = Vec::new();
        area.show(ui, |ui| {
            let Some(doc) = self.doc.as_ref() else {
                return;
            };
            let perfs = self
                .live
                .iter()
                .map(|l| (l.block, &l.perf))
                .chain(self.closed.iter().map(|(b, p)| (*b, p)))
                .collect();
            let view = BlockView {
                perfs,
                armed: self.armed,
                active: doc.active,
                selected: doc.selected,
                playing: self.playing(),
                position: self.position_at(now),
                count_in: self.count_in_beat_at(now),
            };
            let mut block = 0;
            for segment in &doc.lesson.segments {
                match segment {
                    Segment::Text(text) => {
                        CommonMarkViewer::new().show(ui, &mut self.cache, text);
                    }
                    Segment::Rhythm(ex) => {
                        let state = &doc.blocks[block];
                        let then = (ex.next == Next::Attacca)
                            .then(|| doc.block(block + 1))
                            .flatten()
                            .map(|b| b.label.as_str());
                        let resp = rhythm_block(ui, ex, then, block, state, &view, &mut actions);
                        match scroll {
                            Some(Scroll::Block(b)) if b == block => {
                                resp.scroll_to_me(Some(Align::TOP));
                            }
                            Some(Scroll::Reveal(b)) if b == block => resp.scroll_to_me(None),
                            _ => {}
                        }
                        block += 1;
                    }
                }
                ui.add_space(6.0);
            }
        });
        for action in actions {
            let r = match action {
                Action::Play(b) => self.resume(b),
                Action::Stop => self.stop_active(),
                Action::Mute(b, v) => self.toggle_mute(b, v),
            };
            self.note(r);
        }
    }
}

const DIGITS: [Key; MAX_VOICES] = [
    Key::Num1,
    Key::Num2,
    Key::Num3,
    Key::Num4,
    Key::Num5,
    Key::Num6,
    Key::Num7,
    Key::Num8,
    Key::Num9,
];

/// The window as eframe drives it: the app behind the mutex it shares with
/// the IPC connection threads, locked for each call.
pub struct Window(pub Arc<Mutex<App>>);

impl eframe::App for Window {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        lock(&self.0).tick(ctx, Mono::now());
    }

    fn on_exit(&mut self) {
        lock(&self.0).quit();
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        lock(&self.0).frame(ui);
    }
}

/// Starts the thread that serves `app` every `REPAINT`. Queueing a score's
/// next link and emitting its events cannot wait for passes: a window
/// hidden on Wayland gets none.
pub fn drive<S>(app: Arc<Mutex<App<S>>>) -> std::io::Result<()>
where
    App<S>: Send + 'static,
{
    std::thread::Builder::new()
        .name("score".into())
        .spawn(move || {
            loop {
                std::thread::sleep(REPAINT);
                lock(&app).serve(Mono::now());
            }
        })
        .map(drop)
}

impl<S> App<S> {
    /// Follows the score, judges strokes, then serves requests, so a report
    /// includes every stroke and close due by `now`. Runs on every pass, on
    /// the IPC connection thread when a request arrives, and every
    /// `REPAINT` on the thread `drive` starts, so the score continues and
    /// its events are emitted whether or not the compositor lets the window
    /// pass.
    pub fn serve(&mut self, now: Mono) {
        self.follow_score(now);
        self.judge_hits(now);
        self.serve_requests();
    }

    /// The work of every pass at `now`, drawn or not (a hidden window passes
    /// without drawing): `serve`; tells the hub whether the window has
    /// focus; keeps passing while something is to be heard or judged.
    fn tick(&mut self, ctx: &egui::Context, now: Mono) {
        self.serve(now);
        let focused = ctx.input(|i| i.raw.focused && i.viewport().visible() != Some(false));
        let animating = self.animating();
        self.input
            .set_focus(focused, animating.then(|| now + FOCUS_STALE));
        if animating {
            ctx.request_repaint_after(REPAINT);
        }
    }

    /// One frame of the window: keys, status line, lesson.
    fn frame(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        self.keys(&ctx);
        egui::Panel::bottom("status").show(ui, |ui| self.status_line(ui));
        egui::CentralPanel::default().show(ui, |ui| {
            if self.doc.is_some() {
                self.lesson_view(ui);
            } else {
                self.empty_view(ui);
            }
        });
        if self.animating() {
            ctx.request_repaint_after(REPAINT);
        }
    }
}

/// What a click in a rhythm block asks for; applied after drawing.
enum Action {
    Play(usize),
    Stop,
    Mute(usize, usize),
}

/// The transport as every block sees it this frame.
struct BlockView<'a> {
    /// The open performances, then the closed ones still drawn, each with
    /// its block.
    perfs: Vec<(usize, &'a Performance)>,
    /// The block waiting for `space`.
    armed: Option<usize>,
    active: Option<usize>,
    selected: usize,
    playing: bool,
    position: Option<Position>,
    /// The count-in beat being heard, 1-based.
    count_in: Option<u32>,
}

/// One block's own display state.
#[derive(Clone, Debug, PartialEq)]
struct BlockState {
    /// Which voices are muted.
    muted: [bool; MAX_VOICES],
    /// Whether its grid and necklace are hidden.
    hidden: bool,
}

/// The armed block's header, in place of the count-in.
const ARMED_TEXT: &str = "press space to start";

/// One rhythm block: header, then grid and necklace, or a placeholder line
/// when hidden, in a frame outlined when the block is selected.
fn rhythm_block(
    ui: &mut egui::Ui,
    ex: &Exercise,
    then: Option<&str>,
    block: usize,
    state: &BlockState,
    view: &BlockView,
    actions: &mut Vec<Action>,
) -> egui::Response {
    let perf = view
        .perfs
        .iter()
        .find(|(b, _)| *b == block)
        .map(|&(_, p)| p);
    let muted = &state.muted;
    let is_active = view.active == Some(block);
    let is_selected = view.selected == block;
    let sounding = is_active && view.playing;
    let position = view.position.filter(|_| is_active && ex.playhead);
    let mut frame = egui::Frame::group(ui.style());
    if is_selected {
        frame = frame.stroke(Stroke::new(2.0, ui.visuals().selection.stroke.color));
    }
    frame
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                let button = if sounding { "■" } else { "▶" };
                if ui.button(RichText::new(button).size(16.0)).clicked() {
                    actions.push(if sounding {
                        Action::Stop
                    } else {
                        Action::Play(block)
                    });
                }
                ui.label(RichText::new(&ex.label).strong().size(16.0));
                if let Some(title) = ex.title.as_ref().filter(|t| **t != ex.label) {
                    ui.label(title);
                }
                ui.label(RichText::new(meta_line(ex, then)).weak());
                if let Some(you) = you_text(ex) {
                    ui.label(RichText::new(you).strong());
                }
                if let Some(p) = perf {
                    ui.label(performance_text(p));
                }
                if view.armed == Some(block) {
                    ui.label(RichText::new(ARMED_TEXT).strong().size(16.0));
                } else if sounding && let Some(beat) = view.count_in {
                    ui.label(RichText::new(count_in_text(beat)).strong().size(16.0));
                }
            });
            let rows = ex.voices.len() as f32;
            let height = BEAT_STRIP + rows * ROW_HEIGHT;
            let diameter = height.max(80.0);
            let width = ui.available_width();
            let grid_width = (width - diameter - GAP).max(LABEL_WIDTH + 60.0);
            if state.hidden {
                let (rect, _) =
                    ui.allocate_exact_size(Vec2::new(grid_width, diameter), Sense::hover());
                ui.painter_at(rect).text(
                    rect.left_center(),
                    Align2::LEFT_CENTER,
                    "hidden · press h to reveal",
                    FontId::proportional(13.0),
                    ui.visuals().weak_text_color(),
                );
                return;
            }
            let marks = perf.map(visible_marks).unwrap_or_default();
            ui.horizontal(|ui| {
                let (rect, _) =
                    ui.allocate_exact_size(Vec2::new(grid_width, height), Sense::hover());
                let layers = Layers {
                    muted,
                    position,
                    marks: &marks,
                };
                draw_grid(ui, rect, ex, block, &layers, actions);
                let (rect, _) =
                    ui.allocate_exact_size(Vec2::new(diameter, diameter), Sense::hover());
                draw_necklace(ui, rect, ex, &layers);
            });
        })
        .response
}

/// What a block's grid and necklace show besides the exercise itself.
struct Layers<'a> {
    muted: &'a [bool; MAX_VOICES],
    /// The playhead, on the active block while it plays unless the block
    /// hides it.
    position: Option<Position>,
    /// Judgments with their alpha, from `visible_marks`.
    marks: &'a [(Mark, f32)],
}

fn voice_color(i: usize, muted: bool) -> Color32 {
    let c = PALETTE[i % PALETTE.len()];
    if muted {
        c.gamma_multiply(MUTED_ALPHA)
    } else {
        c
    }
}

fn cycle_secs(ex: &Exercise) -> f64 {
    ex.beats as f64 * 60.0 / ex.tempo
}

fn draw_grid(
    ui: &mut egui::Ui,
    rect: Rect,
    ex: &Exercise,
    block: usize,
    layers: &Layers,
    actions: &mut Vec<Action>,
) {
    let &Layers {
        muted,
        position,
        marks,
    } = layers;
    let painter = ui.painter_at(rect);
    let text = ui.visuals().text_color();
    let weak = ui.visuals().weak_text_color();
    let radius = 9.0;
    let left = rect.left() + LABEL_WIDTH + radius;
    let right = rect.right() - radius;
    let width = right - left;
    let rows_top = rect.top() + BEAT_STRIP;
    let x_at = |frac: f64| left + width * frac as f32;

    for b in 0..ex.beats {
        let x = x_at(b as f64 / ex.beats as f64);
        painter.line_segment(
            [Pos2::new(x, rows_top), Pos2::new(x, rect.bottom())],
            Stroke::new(1.0, weak.gamma_multiply(0.5)),
        );
        painter.text(
            Pos2::new(x, rect.top()),
            Align2::CENTER_TOP,
            (b + 1).to_string(),
            FontId::proportional(11.0),
            weak,
        );
    }

    for (vi, voice) in ex.voices.iter().enumerate() {
        let is_muted = muted.get(vi).copied().unwrap_or(false);
        let top = rows_top + vi as f32 * ROW_HEIGHT;
        let mid = top + ROW_HEIGHT / 2.0;
        let label_rect = Rect::from_min_size(
            Pos2::new(rect.left(), top),
            Vec2::new(LABEL_WIDTH - 4.0, ROW_HEIGHT),
        );
        let resp = ui
            .interact(
                label_rect,
                ui.id().with(("mute", block, vi)),
                Sense::click(),
            )
            .on_hover_text(format!("click or press {} to mute/unmute", vi + 1));
        if resp.clicked() {
            actions.push(Action::Mute(block, vi));
        }
        let label_color = if is_muted {
            text.gamma_multiply(MUTED_ALPHA)
        } else {
            text
        };
        painter.text(
            Pos2::new(rect.left() + 2.0, mid),
            Align2::LEFT_CENTER,
            format!("{} {}", vi + 1, voice.name),
            FontId::proportional(13.0),
            label_color,
        );
        let color = voice_color(vi, is_muted);
        let rest = weak.gamma_multiply(if is_muted { MUTED_ALPHA } else { 1.0 });
        let show_tone = voice.instrument.tones().len() > 1;
        let mut onset = voice.onsets.iter().peekable();
        for p in 0..voice.pulses {
            let frac = p as f64 / voice.pulses as f64;
            let center = Pos2::new(x_at(frac), mid);
            let hit = onset.next_if(|o| o.at == p);
            let Some(o) = hit else {
                painter.circle_filled(center, 1.5, rest);
                continue;
            };
            let r = if o.accent { radius } else { radius - 2.5 };
            let lit = position.map_or(0.0, |pos| flash(pos, frac, cycle_secs(ex)));
            if lit > 0.0 && !is_muted {
                painter.circle_filled(center, r + 5.0 * lit, color.gamma_multiply(0.4 * lit));
            }
            painter.circle_filled(center, r, color);
            if show_tone {
                painter.text(
                    center,
                    Align2::CENTER_CENTER,
                    o.tone.to_string(),
                    FontId::monospace(10.0),
                    Color32::BLACK,
                );
            }
        }
    }

    for (m, alpha) in marks.iter().filter(|(m, _)| m.voice < ex.voices.len()) {
        let mid = rows_top + m.voice as f32 * ROW_HEIGHT + ROW_HEIGHT / 2.0;
        let x = x_at(m.frac).clamp(left - radius, right + radius);
        paint_mark(&painter, Pos2::new(x, mid), m.kind, *alpha, radius + 2.0);
    }

    if let Some(pos) = position {
        let x = x_at(pos.frac);
        painter.line_segment(
            [Pos2::new(x, rows_top), Pos2::new(x, rect.bottom())],
            Stroke::new(2.0, text),
        );
    }
}

/// The point at angle `2π·frac` clockwise from 12 o'clock, `r` from `c`.
fn on_ring(c: Pos2, r: f32, frac: f64) -> Pos2 {
    let a = std::f64::consts::TAU * frac;
    Pos2::new(c.x + r * a.sin() as f32, c.y - r * a.cos() as f32)
}

fn draw_necklace(ui: &egui::Ui, rect: Rect, ex: &Exercise, layers: &Layers) {
    let &Layers {
        muted,
        position,
        marks,
    } = layers;
    let painter = ui.painter_at(rect);
    let text = ui.visuals().text_color();
    let weak = ui.visuals().weak_text_color();
    let c = rect.center();
    let outer = rect.width() / 2.0 - 10.0;
    let n = ex.voices.len().max(1) as f32;
    let step = (outer * 0.75 / n).min(16.0);

    for b in 0..ex.beats {
        let frac = b as f64 / ex.beats as f64;
        painter.line_segment(
            [on_ring(c, outer + 3.0, frac), on_ring(c, outer + 9.0, frac)],
            Stroke::new(1.5, weak),
        );
    }
    for (vi, voice) in ex.voices.iter().enumerate() {
        let is_muted = muted.get(vi).copied().unwrap_or(false);
        let r = outer - vi as f32 * step;
        let color = voice_color(vi, is_muted);
        painter.circle_stroke(c, r, Stroke::new(1.0, weak.gamma_multiply(0.5)));
        for o in &voice.onsets {
            let frac = o.at as f64 / voice.pulses as f64;
            let p = on_ring(c, r, frac);
            let lit = position.map_or(0.0, |pos| flash(pos, frac, cycle_secs(ex)));
            let dot = if o.accent { 5.0 } else { 3.5 };
            if lit > 0.0 && !is_muted {
                painter.circle_filled(p, dot + 4.0 * lit, color.gamma_multiply(0.4 * lit));
            }
            painter.circle_filled(p, dot, color);
        }
    }
    for (m, alpha) in marks.iter().filter(|(m, _)| m.voice < ex.voices.len()) {
        let p = on_ring(c, outer - m.voice as f32 * step, m.frac);
        paint_mark(&painter, p, m.kind, *alpha, 8.0);
    }
    if let Some(pos) = position {
        painter.line_segment([c, on_ring(c, outer, pos.frac)], Stroke::new(2.0, text));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::Callback;
    use crate::timeline::tests::exercise;
    use serde_json::{Value, json};
    use std::sync::Mutex;
    use std::time::Instant;

    fn lesson() -> Lesson {
        Lesson {
            segments: vec![
                Segment::Text("# Two blocks\n".into()),
                Segment::Rhythm(exercise(json!({
                    "label": "A", "tempo": 120, "beats": 4, "cycles": 2, "count_in": 4,
                    "voices": [{ "name": "clave", "instrument": "clave", "pulses": 16,
                                 "onsets": [0, 3, 6, 10, 12] },
                               { "name": "click", "instrument": "click", "pulses": 4,
                                 "onsets": [0, 1, 2, 3], "mute": true }]
                }))),
                Segment::Text("Now B.\n".into()),
                Segment::Rhythm(exercise(json!({
                    "label": "B", "tempo": 90, "beats": 4,
                    "voices": [{ "name": "bell", "instrument": "bell", "pulses": 12,
                                 "onsets": [0, 2, 4, 5, 7, 9, 11] }]
                }))),
            ],
        }
    }

    fn app() -> (App<()>, Callback, mpsc::Sender<Pending>) {
        let (app, cb, _, tx) = app_with(Store::default());
        (app, cb, tx)
    }

    type TestApp = (
        App<()>,
        Callback,
        mpsc::Sender<(Hit, Part)>,
        mpsc::Sender<Pending>,
    );

    /// An app keeping its files in `store`, with the sender standing in for
    /// the input hub.
    fn app_with(store: Store) -> TestApp {
        let (engine, cb) = Engine::unplugged(48_000);
        let bank = Bank::from_fn(|_| Arc::from(vec![1.0f32]));
        let (tx, rx) = mpsc::channel();
        let (hit_tx, hits) = mpsc::channel();
        let app = App::new(
            rx,
            engine,
            bank,
            "/run/test.sock".into(),
            "/sf2".into(),
            hits,
            Arc::new(Shared::default()),
        )
        .with_store(store);
        (app, cb, hit_tx, tx)
    }

    fn key(key: Key) -> egui::Event {
        egui::Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: Default::default(),
        }
    }

    fn show(play: Option<&str>) -> Request {
        Request::Show {
            path: "/abs/lessons/x.md".into(),
            lesson: lesson(),
            play: play.map(String::from),
        }
    }

    fn pending(request: Request, deadline: Instant) -> (Pending, mpsc::Receiver<Response>) {
        let (reply, rx) = mpsc::sync_channel(1);
        (
            Pending {
                request,
                reply,
                deadline,
                events: None,
            },
            rx,
        )
    }

    #[test]
    fn drain_skips_expired_requests() {
        let (tx, rx) = mpsc::channel();
        let now = Instant::now();
        let (late, late_reply) = pending(Request::Stop, now - Duration::from_millis(1));
        let (live, _live_reply) =
            pending(Request::Play { label: None }, now + Duration::from_secs(5));
        tx.send(late).unwrap();
        tx.send(live).unwrap();
        let got = drain(&rx);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].request, Request::Play { label: None });
        // The expired request was dropped without a reply.
        assert!(late_reply.try_recv().is_err());
        assert!(drain(&rx).is_empty());
    }

    #[test]
    fn an_expired_show_does_not_change_the_window() {
        let (mut app, _cb, tx) = app();
        let (late, reply) = pending(show(None), Instant::now() - Duration::from_millis(1));
        tx.send(late).unwrap();
        app.serve_requests();
        assert!(app.doc.is_none());
        assert!(reply.try_recv().is_err());

        let (live, reply) = pending(show(None), Instant::now() + Duration::from_secs(5));
        tx.send(live).unwrap();
        app.serve_requests();
        assert_eq!(
            reply.try_recv().unwrap(),
            Response::ok(vec!["A".into(), "B".into()])
        );
        assert_eq!(app.doc.as_ref().unwrap().path, "/abs/lessons/x.md");
    }

    #[test]
    fn pick_block_errors() {
        assert_eq!(
            pick_block(None, None),
            Err("no lesson loaded; run `djembot show LESSON.md` first".into())
        );
        let l = lesson();
        assert_eq!(pick_block(Some(&l), None), Ok(0));
        assert_eq!(pick_block(Some(&l), Some("B")), Ok(1));
        assert_eq!(pick_block(Some(&l), Some("1")), Ok(0));
        assert_eq!(
            pick_block(Some(&l), Some("C")),
            Err("unknown label \"C\"; blocks: 1 A, 2 B".into())
        );
        let text_only = Lesson {
            segments: vec![Segment::Text("hi".into())],
        };
        assert_eq!(
            pick_block(Some(&text_only), None),
            Err("the lesson has no rhythm blocks".into())
        );
    }

    /// Block A: a conga part on `d`/`f` (voice 2) beside a computer clave;
    /// block B: no part.
    fn player_lesson() -> Request {
        Request::Show {
            path: "/abs/lessons/p.md".into(),
            lesson: Lesson {
                segments: vec![
                    Segment::Rhythm(exercise(json!({
                        "label": "A", "tempo": 120, "beats": 4,
                        "voices": [{ "name": "clave", "instrument": "clave", "pulses": 4,
                                     "onsets": [0] },
                                   { "name": "conga", "instrument": "conga", "pulses": 4,
                                     "onsets": [0, 2], "keys": {"o": "d", "s": "f"},
                                     "gain": 0.5, "pan": 0.25 }]
                    }))),
                    Segment::Rhythm(exercise(json!({
                        "label": "B", "tempo": 90, "beats": 4,
                        "voices": [{ "name": "bell", "instrument": "bell", "pulses": 4,
                                     "onsets": [0] }]
                    }))),
                ],
            },
            play: None,
        }
    }

    fn armed_keys(app: &App<()>) -> Option<Vec<Trigger>> {
        let arming = app.input.arming()?;
        let mut keys: Vec<_> = arming.parts.keys().copied().collect();
        keys.sort_by_key(|t| match t {
            Trigger::Key(c) => *c,
        });
        Some(keys)
    }

    fn key_code(name: &str) -> Trigger {
        Trigger::Key(input::keys::code(name).unwrap())
    }

    #[test]
    fn arming_follows_the_loaded_block() {
        let (mut app, _cb, _tx) = app();
        let Request::Show { path, lesson, .. } = player_lesson() else {
            unreachable!()
        };
        let with_play = |play: Option<&str>| Request::Show {
            path: path.clone(),
            lesson: lesson.clone(),
            play: play.map(String::from),
        };
        app.handle(with_play(Some("A")));
        let mut expected = vec![key_code("d"), key_code("f")];
        expected.sort_by_key(|t| match t {
            Trigger::Key(c) => *c,
        });
        assert_eq!(armed_keys(&app), Some(expected.clone()), "show --play arms");
        let part = app.input.arming().unwrap().parts[&key_code("d")].clone();
        assert_eq!((part.gain, part.pan), (0.5, 0.25));
        assert!(part.buf.is_some(), "a part sounds unless it is silent");

        app.start(1).unwrap();
        assert_eq!(armed_keys(&app), None, "a block without a part disarms");
        app.resume(0).unwrap();
        assert_eq!(armed_keys(&app), Some(expected), "activate arms");

        app.handle(with_play(None));
        assert_eq!(armed_keys(&app), None, "show without play disarms");
        app.handle(with_play(Some("B")));
        assert_eq!(armed_keys(&app), None);
    }

    #[test]
    fn a_silent_voice_arms_without_a_sound() {
        let ex = exercise(json!({
            "tempo": 100, "beats": 4,
            "voices": [{ "name": "click", "instrument": "click", "pulses": 4,
                         "onsets": [0, 1, 2, 3], "keys": "f", "silent": true }]
        }));
        let mut bank = Bank::from_fn(|id| panic!("a silent part loads no sound: {id:?}"));
        let arming = arming([&ex], &mut bank).unwrap().expect("a player part");
        let part = &arming.parts[&key_code("f")];
        assert_eq!(part_keys(&ex)[&key_code("f")], (0, 'x'));
        assert!(part.buf.is_none() && part.sound().is_none());
    }

    /// Raw input for a pass of the root window with `focused`, minimised
    /// or not.
    fn window_input(focused: bool, minimized: bool) -> egui::RawInput {
        let mut input = egui::RawInput {
            focused,
            ..Default::default()
        };
        input.viewports.insert(
            egui::ViewportId::ROOT,
            egui::ViewportInfo {
                minimized: Some(minimized),
                occluded: Some(false),
                ..Default::default()
            },
        );
        input
    }

    #[test]
    fn every_pass_tells_the_hub_whether_the_window_has_focus() {
        let (mut app, mut cb, _tx) = app();
        let ctx = egui::Context::default();
        let now = Mono(1_000_000_000);
        let drawn = |app: &mut App<()>, focused: bool| {
            ctx.run_ui(window_input(focused, false), |ui| app.tick(ui.ctx(), now))
                .textures_delta
                .clear();
        };
        for focused in [true, false, true] {
            drawn(&mut app, focused);
            assert_eq!(app.input.focused_at(now), focused);
        }
        assert!(
            app.input.focused_at(now + Duration::from_secs(3600)),
            "idle: focus holds until the next pass"
        );

        // Passes that are not drawn: minimised, with or without focus.
        let hidden = |app: &mut App<()>, focused: bool| {
            let _ = ctx.run_logic(&window_input(focused, true), |ctx| app.tick(ctx, now));
        };
        hidden(&mut app, false);
        assert!(!app.input.focused_at(now));
        drawn(&mut app, true);
        hidden(&mut app, true);
        assert!(!app.input.focused_at(now), "a hidden window is not played");

        // Playing, the window expects to pass again soon; focus from a pass
        // older than FOCUS_STALE no longer counts.
        app.handle(show(Some("A")));
        cb.tick();
        drawn(&mut app, true);
        assert!(app.input.focused_at(now + FOCUS_STALE));
        assert!(
            !app.input
                .focused_at(now + FOCUS_STALE + Duration::from_nanos(1))
        );
    }

    #[test]
    fn a_pass_that_is_not_drawn_keeps_passing_while_playing() {
        let (mut app, mut cb, _tx) = app();
        let ctx = egui::Context::default();
        let delays = Arc::new(Mutex::new(Vec::new()));
        let seen = delays.clone();
        ctx.set_request_repaint_callback(move |info| seen.lock().unwrap().push(info.delay));
        let hidden = |app: &mut App<()>| {
            delays.lock().unwrap().clear();
            let _ = ctx.run_logic(&window_input(false, true), |ctx| app.tick(ctx, Mono::now()));
            delays.lock().unwrap().iter().any(|d| *d <= REPAINT)
        };
        app.handle(show(None));
        cb.tick();
        // A new context asks for its own first passes.
        hidden(&mut app);
        assert!(!hidden(&mut app), "idle");
        app.handle(Request::Play { label: None });
        cb.tick();
        assert!(hidden(&mut app), "playing");
    }

    #[test]
    fn show_with_play_loads_and_starts_the_block() {
        let (mut app, mut cb, _tx) = app();
        assert_eq!(
            app.handle(show(Some("B"))),
            Response::ok(vec!["A".into(), "B".into()])
        );
        assert_eq!(app.active_block(), Some(1));
        assert_eq!(app.scroll, Some(Scroll::Block(1)));
        cb.tick();
        assert!(app.playing());
        assert!(matches!(app.transport(), Transport::Playing(_)));
    }

    #[test]
    fn show_stops_playback_and_scrolls_to_the_top() {
        let (mut app, mut cb, _tx) = app();
        app.handle(show(Some("A")));
        cb.tick();
        assert!(app.playing());
        assert!(app.handle(show(None)).ok);
        cb.tick();
        assert!(!app.playing());
        assert_eq!(app.active_block(), None);
        assert_eq!(app.scroll, Some(Scroll::Top));
        assert_eq!(app.transport(), Transport::Stopped);
    }

    #[test]
    fn show_with_an_unknown_label_changes_nothing() {
        let (mut app, mut cb, _tx) = app();
        let r = app.handle(show(Some("Z")));
        assert_eq!(r, Response::err("unknown label \"Z\"; blocks: 1 A, 2 B"));
        assert!(app.doc.is_none());

        app.handle(Request::Show {
            path: "/abs/lessons/old.md".into(),
            lesson: lesson(),
            play: Some("A".into()),
        });
        cb.tick();
        app.scroll = None;
        assert!(!app.handle(show(Some("Z"))).ok);
        cb.tick();
        assert_eq!(app.doc.as_ref().unwrap().path, "/abs/lessons/old.md");
        assert_eq!(app.active_block(), Some(0));
        assert!(app.playing());
        assert_eq!(app.scroll, None);
    }

    #[test]
    fn play_and_stop_requests() {
        let (mut app, mut cb, _tx) = app();
        assert_eq!(
            app.handle(Request::Play { label: None }),
            Response::err("no lesson loaded; run `djembot show LESSON.md` first")
        );
        assert!(
            app.handle(Request::Stop).ok,
            "stop with nothing loaded is fine"
        );
        app.handle(show(None));
        assert!(app.handle(Request::Play { label: None }).ok);
        assert_eq!(app.active_block(), Some(0));
        cb.tick();
        // Block A has a count-in, so the transport counts in first.
        assert_eq!(app.transport(), Transport::CountIn);
        assert_eq!(app.count_in_beat_now(), Some(1));
        assert!(app.handle(Request::Stop).ok);
        cb.tick();
        assert!(!app.playing());
        assert_eq!(app.transport(), Transport::Stopped);
        // Switching blocks loads the other block.
        assert!(
            app.handle(Request::Play {
                label: Some("2".into())
            })
            .ok
        );
        assert_eq!(app.active_block(), Some(1));
        assert_eq!(app.scroll, Some(Scroll::Block(1)));
    }

    /// Fills the engine's command ring.
    fn fill_ring(app: &mut App<()>) {
        while app.engine.set_mute(0, false, 0).is_ok() {}
    }

    fn another(play: Option<&str>) -> Request {
        Request::Show {
            path: "/abs/lessons/new.md".into(),
            lesson: lesson(),
            play: play.map(String::from),
        }
    }

    /// A full queue while A plays: neither `show --play` nor `show` fits.
    #[test]
    fn a_show_that_does_not_fit_the_queue_changes_nothing() {
        let (mut app, mut cb, _tx) = app();
        app.handle(show(Some("A")));
        cb.tick();
        app.scroll = None;
        fill_ring(&mut app);
        let r = app.handle(another(Some("B")));
        assert!(r.error.unwrap().contains("queue is full"));
        assert!(!app.handle(another(None)).ok);
        assert_eq!(app.doc.as_ref().unwrap().path, "/abs/lessons/x.md");
        assert_eq!(app.active_block(), Some(0));
        assert_eq!(app.scroll, None);
        cb.tick();
        assert!(app.playing());
    }

    /// A dead stream and nothing active: the text still shows, but a
    /// `--play` fails before the document is replaced.
    #[test]
    fn a_show_play_on_a_dead_stream_changes_nothing() {
        let (mut app, _cb, _tx) = app();
        app.handle(show(None));
        app.engine.kill("audio stream error: gone");
        let r = app.handle(another(Some("B")));
        assert!(r.error.unwrap().contains("audio has stopped"));
        assert_eq!(app.doc.as_ref().unwrap().path, "/abs/lessons/x.md");
        assert_eq!(app.active_block(), None);
        assert!(app.handle(another(None)).ok);
        assert_eq!(app.doc.as_ref().unwrap().path, "/abs/lessons/new.md");
    }

    #[test]
    fn a_play_the_engine_cannot_start_changes_nothing() {
        let (mut app, _cb, _tx) = app();
        app.handle(show(None));
        fill_ring(&mut app);
        assert!(!app.handle(Request::Play { label: None }).ok);
        assert_eq!(app.active_block(), None);
        assert!(app.resume(1).is_err());
        assert_eq!(app.active_block(), None);
    }

    #[test]
    fn mutes_follow_the_block_and_reach_the_timeline() {
        let (mut app, mut cb, _tx) = app();
        app.handle(show(None));
        let doc = app.doc.as_ref().unwrap();
        assert_eq!(
            doc.blocks[0].muted[..2],
            [false, true],
            "from the voices' mute"
        );
        // Muting before a start is carried into the loaded timeline.
        app.toggle_mute(0, 0).unwrap();
        app.start(0).unwrap();
        assert_eq!(app.chain[0].tl.mute[..2], [true, true]);
        app.toggle_mute(0, 1).unwrap();
        assert_eq!(
            app.doc.as_ref().unwrap().blocks[0].muted[..2],
            [true, false]
        );
        // A voice the block lacks, or a block the lesson lacks: nothing
        // changes and nothing is sent.
        cb.tick();
        let before = app.doc.as_ref().unwrap().blocks.clone();
        app.toggle_mute(0, 2).unwrap();
        app.toggle_mute(0, 8).unwrap();
        app.toggle_mute(5, 0).unwrap();
        assert_eq!(app.doc.as_ref().unwrap().blocks, before);
        assert!(!app.engine.commands_pending());
    }

    #[test]
    fn muting_the_playing_block_silences_the_voice() {
        // B's only voice strikes at frame 0 of the first callback.
        let (mut plain, mut plain_cb, _plain_tx) = app();
        plain.handle(show(Some("B")));
        assert!(
            plain_cb.tick().iter().any(|&x| x != 0.0),
            "unmuted: audible"
        );

        let (mut app, mut cb, _tx) = app();
        app.handle(show(Some("B")));
        app.toggle_mute(1, 0).unwrap();
        assert!(app.engine.commands_pending());
        assert!(cb.tick().iter().all(|&x| x == 0.0), "muted: silent");
        assert!(app.playing());
    }

    /// Ticks the callback until the active block is past its count-in.
    fn tick_into_playing(app: &App<()>, cb: &mut Callback) {
        for _ in 0..1000 {
            if matches!(app.transport(), Transport::Playing(_)) {
                return;
            }
            cb.tick();
        }
        panic!("never left the count-in");
    }

    #[test]
    fn replaying_the_playing_block_restarts_the_count_in() {
        let (mut app, mut cb, _tx) = app();
        app.handle(show(Some("A")));
        tick_into_playing(&app, &mut cb);
        assert!(
            app.handle(Request::Play {
                label: Some("A".into())
            })
            .ok
        );
        cb.tick();
        assert_eq!(app.transport(), Transport::CountIn);
        assert_eq!(app.count_in_beat_now(), Some(1));

        tick_into_playing(&app, &mut cb);
        let ctx = egui::Context::default();
        let input = egui::RawInput {
            events: vec![key(Key::R)],
            ..Default::default()
        };
        ctx.run_ui(input, |ui| app.frame(ui)).textures_delta.clear();
        cb.tick();
        assert_eq!(app.transport(), Transport::CountIn, "r restarts");
        assert_eq!(app.count_in_beat_now(), Some(1));
    }

    #[test]
    fn paused_is_distinct_from_stopped() {
        let (mut app, mut cb, _tx) = app();
        app.handle(show(Some("B")));
        cb.tick();
        app.engine.pause().unwrap();
        cb.tick();
        assert_eq!(app.transport(), Transport::Paused);
        let paused_at = app.engine.clock().frame;
        assert!(paused_at > 0);
        app.resume(1).unwrap();
        cb.tick();
        assert!(app.playing());
        assert!(app.engine.clock().frame >= paused_at, "resumed, not reset");
    }

    #[test]
    fn resuming_a_finished_block_plays_it_again() {
        let (mut app, mut cb, _tx) = app();
        app.handle(show(Some("A")));
        cb.tick();
        for _ in 0..2000 {
            if !app.playing() {
                break;
            }
            cb.tick();
        }
        assert!(!app.playing(), "A has two cycles, then stops");
        assert_eq!(app.transport(), Transport::Stopped);
        app.resume(0).unwrap();
        cb.tick();
        assert_eq!(app.transport(), Transport::CountIn);
        assert_eq!(app.count_in_beat_now(), Some(1));
    }

    #[test]
    fn space_on_an_idle_lesson_plays_the_first_block() {
        let (mut app, mut cb, _tx) = app();
        app.handle(show(None));
        assert_eq!(app.doc.as_ref().unwrap().selected, 0);
        let ctx = egui::Context::default();
        let input = egui::RawInput {
            events: vec![key(Key::Space)],
            ..Default::default()
        };
        ctx.run_ui(input, |ui| app.frame(ui)).textures_delta.clear();
        assert_eq!(app.active_block(), Some(0));
        cb.tick();
        assert!(app.playing());
    }

    /// Runs one frame with `keys` pressed.
    fn press(app: &mut App<()>, ctx: &egui::Context, keys: &[Key]) -> egui::FullOutput {
        let input = egui::RawInput {
            events: keys.iter().map(|k| key(*k)).collect(),
            ..Default::default()
        };
        let mut out = ctx.run_ui(input, |ui| app.frame(ui));
        out.textures_delta.clear();
        out
    }

    /// Handles `keys` without drawing, so `scroll` is left for inspection.
    fn keys_only(app: &mut App<()>, ctx: &egui::Context, keys: &[Key]) {
        let input = egui::RawInput {
            events: keys.iter().map(|k| key(*k)).collect(),
            ..Default::default()
        };
        ctx.run_ui(input, |ui| app.keys(ui.ctx()))
            .textures_delta
            .clear();
    }

    fn selected(app: &App<()>) -> usize {
        app.doc.as_ref().unwrap().selected
    }

    #[test]
    fn j_and_k_move_the_selection_without_playing() {
        let (mut app, mut cb, _tx) = app();
        let ctx = egui::Context::default();
        app.handle(show(None));
        keys_only(&mut app, &ctx, &[Key::J]);
        assert_eq!(selected(&app), 1);
        assert_eq!(app.scroll, Some(Scroll::Reveal(1)));
        assert_eq!(app.active_block(), None);
        cb.tick();
        assert!(!app.playing());
        press(&mut app, &ctx, &[Key::J]);
        assert_eq!(selected(&app), 1, "clamped at the last block");
        keys_only(&mut app, &ctx, &[Key::ArrowUp]);
        assert_eq!(selected(&app), 0);
        assert_eq!(app.scroll, Some(Scroll::Reveal(0)));
        press(&mut app, &ctx, &[Key::K]);
        assert_eq!(selected(&app), 0, "clamped at the first block");
        press(&mut app, &ctx, &[Key::ArrowDown]);
        assert_eq!(selected(&app), 1);
        assert_eq!(app.active_block(), None);
        assert!(!app.engine.commands_pending(), "selecting sends nothing");
    }

    #[test]
    fn space_plays_the_selected_block_and_stops_the_active_one() {
        let (mut app, mut cb, _tx) = app();
        let ctx = egui::Context::default();
        app.handle(show(None));
        app.handle(Request::Play {
            label: Some("A".into()),
        });
        cb.tick();
        assert!(app.playing());
        press(&mut app, &ctx, &[Key::J]);
        assert_eq!(app.active_block(), Some(0), "A keeps playing");
        cb.tick();
        assert!(app.playing());
        press(&mut app, &ctx, &[Key::Space]);
        cb.tick();
        assert_eq!(app.active_block(), Some(1));
        assert_eq!(selected(&app), 1);
        // B has no count-in.
        match app.transport() {
            Transport::Playing(p) => assert_eq!(p.cycle, 0),
            t => panic!("{t:?}"),
        }
    }

    #[test]
    fn space_pauses_only_the_selected_active_block() {
        let (mut app, mut cb, _tx) = app();
        let ctx = egui::Context::default();
        app.handle(show(Some("A")));
        cb.tick();
        press(&mut app, &ctx, &[Key::Space]);
        cb.tick();
        assert_eq!(app.transport(), Transport::Paused);
        press(&mut app, &ctx, &[Key::Space]);
        cb.tick();
        assert!(app.playing());
        assert_eq!(app.active_block(), Some(0));
    }

    #[test]
    fn playing_a_block_selects_it() {
        let (mut app, _cb, _tx) = app();
        app.handle(show(None));
        assert!(
            app.handle(Request::Play {
                label: Some("B".into())
            })
            .ok
        );
        assert_eq!(selected(&app), 1);

        let (mut shown, _cb, _tx) = super::tests::app();
        shown.handle(show(Some("B")));
        assert_eq!(selected(&shown), 1, "show --play");

        let (mut app, _cb, _tx) = super::tests::app();
        app.handle(show(None));
        // What the ▶ button's `Action::Play(1)` does.
        app.resume(1).unwrap();
        assert_eq!(selected(&app), 1);
        // Moved away, then replayed while still active: selected again.
        app.doc.as_mut().unwrap().selected = 0;
        app.handle(Request::Play {
            label: Some("B".into()),
        });
        assert_eq!(selected(&app), 1);
    }

    #[test]
    fn digits_and_r_act_on_the_selected_block() {
        let (mut app, mut cb, _tx) = app();
        let ctx = egui::Context::default();
        app.handle(show(None));
        press(&mut app, &ctx, &[Key::J]);
        press(&mut app, &ctx, &[Key::Num1]);
        let doc = app.doc.as_ref().unwrap();
        assert!(doc.blocks[1].muted[0], "B's voice 1");
        assert!(!doc.blocks[0].muted[0], "not A's");
        press(&mut app, &ctx, &[Key::R]);
        assert_eq!(app.active_block(), Some(1));
        cb.tick();
        assert!(app.playing());
    }

    #[test]
    fn handled_keys_are_consumed() {
        let (mut app, _cb, _tx) = app();
        let ctx = egui::Context::default();
        app.handle(show(None));
        let digits = [
            Key::Num1,
            Key::Num2,
            Key::Num3,
            Key::Num4,
            Key::Num5,
            Key::Num6,
            Key::Num7,
            Key::Num8,
            Key::Num9,
        ];
        let moves = [Key::Space, Key::J, Key::K, Key::ArrowDown, Key::ArrowUp];
        for k in moves.into_iter().chain([Key::R, Key::H]).chain(digits) {
            let input = egui::RawInput {
                events: vec![key(k)],
                ..Default::default()
            };
            ctx.run_ui(input, |ui| {
                app.frame(ui);
                assert!(!ui.input(|i| i.key_pressed(k)), "{k:?} reached the widgets");
            })
            .textures_delta
            .clear();
        }
    }

    fn hidden_lesson() -> Lesson {
        let mut l = lesson();
        let Segment::Rhythm(ex) = &mut l.segments[1] else {
            panic!("block A");
        };
        let mut v = Value::from(ex.clone());
        v["hidden"] = true.into();
        *ex = exercise(v);
        l
    }

    /// Each block's hidden flag, in block order.
    fn hidden<S>(app: &App<S>) -> Vec<bool> {
        let doc = app.doc.as_ref().unwrap();
        doc.blocks.iter().map(|b| b.hidden).collect()
    }

    #[test]
    fn hidden_blocks_draw_without_a_grid() {
        let (mut app, mut cb, _tx) = app();
        let ctx = egui::Context::default();
        app.handle(Request::Show {
            path: "/abs/lessons/hidden.md".into(),
            lesson: hidden_lesson(),
            play: None,
        });
        assert_eq!(hidden(&app), [true, false]);
        let placeholder = "hidden · press h to reveal";
        assert_eq!(texts(&press(&mut app, &ctx, &[]), placeholder), 1);
        press(&mut app, &ctx, &[Key::Space]);
        cb.tick();
        let hidden_shapes = press(&mut app, &ctx, &[]).shapes.len();
        press(&mut app, &ctx, &[Key::H]);
        assert_eq!(hidden(&app), [false, false]);
        let out = press(&mut app, &ctx, &[]);
        assert_eq!(texts(&out, placeholder), 0);
        assert!(out.shapes.len() > hidden_shapes, "the grid is drawn again");
        press(&mut app, &ctx, &[Key::H]);
        assert_eq!(hidden(&app), [true, false]);
        assert_eq!(texts(&press(&mut app, &ctx, &[]), placeholder), 1);
        press(&mut app, &ctx, &[Key::Num1]);
        assert!(
            app.doc.as_ref().unwrap().blocks[0].muted[0],
            "1 still mutes"
        );
        // A new `show` takes the hidden state from the lesson again.
        app.handle(show(None));
        assert_eq!(hidden(&app), [false, false]);
    }

    /// The number of text shapes whose text is exactly `text`.
    fn texts(out: &egui::FullOutput, text: &str) -> usize {
        out.shapes
            .iter()
            .filter(|c| matches!(&c.shape, egui::Shape::Text(t) if t.galley.text() == text))
            .count()
    }

    /// The rectangles drawn with a 2 px stroke in the selection colour.
    fn outlines(ctx: &egui::Context, out: &egui::FullOutput) -> Vec<egui::Rect> {
        let color = ctx.global_style().visuals.selection.stroke.color;
        out.shapes
            .iter()
            .filter_map(|c| match &c.shape {
                egui::Shape::Rect(r) if r.stroke.width == 2.0 && r.stroke.color == color => {
                    Some(r.rect)
                }
                _ => None,
            })
            .collect()
    }

    #[test]
    fn selected_block_is_highlighted() {
        let (mut app, _cb, _tx) = app();
        let ctx = egui::Context::default();
        app.handle(show(None));
        let first = outlines(&ctx, &press(&mut app, &ctx, &[]));
        assert_eq!(first.len(), 1);
        press(&mut app, &ctx, &[Key::J]);
        let second = outlines(&ctx, &press(&mut app, &ctx, &[]));
        assert_eq!(second.len(), 1);
        assert!(second[0].min.y > first[0].min.y, "the outline moved to B");
    }

    #[test]
    fn key_failures_reach_the_status_line() {
        let (mut app, _cb, _tx) = app();
        app.handle(show(None));
        let ctx = egui::Context::default();
        let press = |app: &mut App<()>, k| {
            let input = egui::RawInput {
                events: vec![key(k)],
                ..Default::default()
            };
            ctx.run_ui(input, |ui| app.frame(ui)).textures_delta.clear();
        };
        fill_ring(&mut app);
        press(&mut app, Key::R);
        let notice = app.notice.clone().unwrap();
        assert!(notice.contains("queue is full"), "{notice}");
        assert_eq!(app.problem(), Some(notice));
        // A new lesson clears it.
        app.handle(show(None));
        assert_eq!(app.notice, None);
        assert_eq!(app.problem(), None);
        // A dead stream outranks a notice.
        app.notice = Some("older".into());
        app.engine.kill("audio stream error: gone");
        assert_eq!(
            app.problem().unwrap(),
            "audio stream error: gone; restart `djembot gui`"
        );
    }

    /// A play request keeps the window redrawing although the callback has
    /// not applied it yet, for a fresh block and for a replayed one.
    #[test]
    fn a_play_not_yet_applied_keeps_repainting() {
        let (mut app, mut cb, _tx) = app();
        let ctx = egui::Context::default();
        // The repaint delay the third of three frames asks for: egui's own
        // first frames, and the scroll animation, which is cleared, would
        // otherwise request frames themselves.
        let repaint = |app: &mut App<()>| {
            let mut delay = Duration::ZERO;
            for _ in 0..3 {
                app.scroll = None;
                let mut out = ctx.run_ui(egui::RawInput::default(), |ui| app.frame(ui));
                out.textures_delta.clear();
                delay = out.viewport_output[&egui::ViewportId::ROOT].repaint_delay;
            }
            delay
        };
        app.handle(show(None));
        cb.tick();
        assert!(repaint(&mut app) > REPAINT, "idle: no repaint loop");

        app.handle(Request::Play { label: None });
        assert!(!app.playing(), "the callback has not run");
        assert!(repaint(&mut app) <= REPAINT);
        cb.tick();
        assert!(app.playing());
        assert!(repaint(&mut app) <= REPAINT);

        app.handle(Request::Stop);
        cb.tick();
        assert!(repaint(&mut app) > REPAINT, "stopped: no repaint loop");
        app.handle(Request::Play { label: None });
        assert!(!app.playing(), "the bare Play is not applied yet");
        assert!(repaint(&mut app) <= REPAINT);
    }

    /// Draws frames headlessly: empty, then the lesson stopped, counting
    /// in, and playing with a muted voice, then `q`.
    #[test]
    fn frames_draw_without_a_display() {
        let (mut app, mut cb, _tx) = app();
        let ctx = egui::Context::default();
        let run = |app: &mut App<()>, events: Vec<egui::Event>| {
            let input = egui::RawInput {
                events,
                ..Default::default()
            };
            let mut out = ctx.run_ui(input, |ui| app.frame(ui));
            out.textures_delta.clear();
            out
        };
        run(&mut app, vec![]);
        app.handle(show(None));
        run(&mut app, vec![]);
        app.handle(Request::Play { label: None });
        cb.tick();
        run(&mut app, vec![]);
        app.handle(Request::Play {
            label: Some("B".into()),
        });
        cb.tick();
        run(&mut app, vec![key(Key::Num1)]);
        assert!(
            app.doc.as_ref().unwrap().blocks[1].muted[0],
            "1 mutes voice 1"
        );
        run(&mut app, vec![key(Key::Space)]);
        cb.tick();
        assert_eq!(app.transport(), Transport::Paused, "space pauses");
        let out = run(&mut app, vec![key(Key::Q)]);
        let root = out.viewport_output.get(&egui::ViewportId::ROOT).unwrap();
        assert!(root.commands.contains(&egui::ViewportCommand::Close));
    }

    #[test]
    fn flash_decays_after_the_playhead_passes() {
        let at = |cycle, frac| Position { cycle, frac };
        // A 2 s cycle: 150 ms is 0.075 of it.
        assert_eq!(flash(at(0, 0.25), 0.25, 2.0), 1.0);
        assert!((flash(at(0, 0.25 + 0.0375), 0.25, 2.0) - 0.5).abs() < 1e-6);
        assert_eq!(flash(at(0, 0.25 + 0.075), 0.25, 2.0), 0.0);
        assert_eq!(flash(at(0, 0.2), 0.25, 2.0), 0.0, "not yet reached");
        // Pulse 0 just after the wrap into cycle 1 is lit; in cycle 0 the
        // end of the cycle has not happened yet.
        assert!(flash(at(1, 0.01), 0.0, 2.0) > 0.0);
        assert!(flash(at(1, 0.01), 0.99, 2.0) > 0.0);
        assert_eq!(flash(at(0, 0.01), 0.99, 2.0), 0.0);
    }

    #[test]
    fn header_and_status_text() {
        let l = lesson();
        let blocks: Vec<&Exercise> = l.blocks().collect();
        assert_eq!(meta_line(blocks[0], None), "120 bpm · 4 beats · 2 cycles");
        assert_eq!(meta_line(blocks[1], None), "90 bpm · 4 beats · ∞");
        assert_eq!(
            meta_line(blocks[0], Some("B")),
            "120 bpm · 4 beats · 2 cycles · then B"
        );
        assert_eq!(count_in_text(1), "1");
        assert_eq!(count_in_text(4), "1 · 2 · 3 · 4");
        let p = Position {
            cycle: 2,
            frac: 0.5,
        };
        assert_eq!(
            transport_text(Some("A"), Some(8), Transport::Playing(p)),
            "playing A · cycle 3/8"
        );
        assert_eq!(
            transport_text(Some("A"), None, Transport::Playing(p)),
            "playing A · cycle 3 ∞"
        );
        assert_eq!(transport_text(None, None, Transport::Stopped), "stopped");
        assert_eq!(
            transport_text(Some("B"), None, Transport::Paused),
            "paused B"
        );
        assert_eq!(
            transport_text(Some("B"), None, Transport::CountIn),
            "count-in B"
        );
    }

    const RATE: f64 = 48_000.0;
    /// Frames per 256-frame tick, in seconds.
    const TICK: f64 = 256.0 / RATE;

    /// Block P: one cycle at 120 bpm, no count-in, a clave on `f` at
    /// pulses 0 and 15 of 16 (the last a 16th, 125 ms, before the end at
    /// frame 96 000), window 100 ms. Q: the same part, looping. N: no part.
    fn perf_lesson(path: &str) -> Request {
        let part = |cycles: Option<u32>| {
            let mut v = json!({
                "tempo": 120, "beats": 4, "window_ms": 100,
                "voices": [{ "name": "clave", "instrument": "clave", "pulses": 16,
                             "onsets": [0, 15], "keys": "f" }]
            });
            if let Some(c) = cycles {
                v["cycles"] = c.into();
            }
            v
        };
        let mut p = part(Some(1));
        p["label"] = "P".into();
        let mut q = part(None);
        q["label"] = "Q".into();
        Request::Show {
            path: path.into(),
            lesson: Lesson {
                segments: vec![
                    Segment::Rhythm(exercise(p)),
                    Segment::Rhythm(exercise(q)),
                    Segment::Rhythm(exercise(json!({
                        "label": "N", "tempo": 90, "beats": 4,
                        "voices": [{ "name": "bell", "instrument": "bell", "pulses": 4,
                                     "onsets": [0] }]
                    }))),
                ],
            },
            play: None,
        }
    }

    /// A score at 120 bpm (a 96 000-frame cycle): `guided`, two cycles
    /// after a four-beat count-in, a clave and a click, then attacca
    /// `alone`, two cycles with a count-in of its own, then — when `coda` —
    /// attacca `coda`, one cycle of bell.
    fn score_lesson(coda: bool) -> Request {
        let block = |label: &str, cycles: u32, next: &str| {
            json!({
                "label": label, "tempo": 120, "beats": 4, "cycles": cycles, "count_in": 4,
                "next": next,
                "voices": [{ "name": "clave", "instrument": "clave", "pulses": 16,
                             "onsets": [0, 3, 6, 10, 12] },
                           { "name": "click", "instrument": "click", "pulses": 4,
                             "onsets": [0, 1, 2, 3] }]
            })
        };
        let mut segments = vec![
            Segment::Rhythm(exercise(block("guided", 2, "attacca"))),
            Segment::Text("Then alone.\n".into()),
            Segment::Rhythm(exercise(block(
                "alone",
                2,
                if coda { "attacca" } else { "stop" },
            ))),
        ];
        if coda {
            segments.push(Segment::Rhythm(exercise(json!({
                "label": "coda", "tempo": 120, "beats": 4, "cycles": 1,
                "voices": [{ "name": "bell", "instrument": "bell", "pulses": 4,
                             "onsets": [0] }]
            }))));
        }
        Request::Show {
            path: "/abs/lessons/score.md".into(),
            lesson: Lesson { segments },
            play: None,
        }
    }

    #[test]
    fn score_follows_attacca_to_the_first_stop() {
        let blocks = |lesson: &Lesson, b| score(lesson, b).map(|(i, _)| i).collect::<Vec<_>>();
        let Request::Show { lesson, .. } = score_lesson(true) else {
            unreachable!()
        };
        assert_eq!(blocks(&lesson, 0), [0, 1, 2]);
        assert_eq!(blocks(&lesson, 1), [1, 2]);
        assert_eq!(blocks(&lesson, 2), [2]);
        assert!(score(&lesson, 3).next().is_none());
        let Request::Show { lesson, .. } = score_lesson(false) else {
            unreachable!()
        };
        assert_eq!(blocks(&lesson, 0), [0, 1]);
        assert_eq!(blocks(&lesson, 1), [1]);
    }

    #[test]
    fn a_score_is_compiled_on_one_clock_with_later_count_ins_dropped() {
        let (mut app, _cb, _tx) = app();
        app.handle(score_lesson(true));
        app.start(0).unwrap();
        let links: Vec<_> = app
            .chain
            .iter()
            .map(|l| (l.block, l.origin, l.tl.count_in_len, l.tl.end))
            .collect();
        assert_eq!(
            links,
            [
                (0, 0, 96_000, Some(288_000)),
                (1, 288_000, 0, Some(192_000)),
                (2, 480_000, 0, Some(96_000)),
            ]
        );
        // Only the last link closes on the downbeat.
        assert!(app.chain[0].tl.closing.is_empty() && app.chain[1].tl.closing.is_empty());
        assert!(!app.chain[2].tl.closing.is_empty());
        assert_eq!(app.queued, 1, "the second link is queued with the head");
    }

    #[test]
    fn a_score_plays_through_its_blocks_on_one_clock() {
        let (mut app, mut cb, _hit_tx, _tx) = app_with(Store::default());
        app.handle(score_lesson(true));
        let r = app.handle(Request::Play {
            label: Some("guided".into()),
        });
        assert!(r.ok, "{r:?}");
        let mut sim = Sim::new(20);
        let mut lines = Vec::new();
        let mut at_swap = None;
        let mut moved = None;
        while sim.k < 3000 {
            let t = sim.at(sim.k as f64 * TICK);
            cb.tick_at(t, sim.latency);
            sim.k += 1;
            app.follow_score(t);
            app.judge_hits(t);
            let doc = app.doc.as_ref().unwrap();
            let line = app.transport_line(doc, t);
            let segment = app.engine.clock().segment;
            if segment == 1 && at_swap.is_none() {
                at_swap = Some(line.clone());
            }
            if doc.active == Some(1) && moved.is_none() {
                moved = Some((line.clone(), doc.selected, app.scroll.take()));
            }
            lines.push(line);
            if !app.engine.clock().playing {
                break;
            }
        }
        // The rendered clock swaps first; the user still hears `guided`.
        assert_eq!(at_swap.as_deref(), Some("playing guided · cycle 2/2"));
        assert_eq!(
            moved,
            Some((
                "playing alone · cycle 1/2".to_string(),
                1,
                Some(Scroll::Block(1))
            ))
        );
        assert!(!lines.iter().any(|l| l.starts_with("count-in alone")));
        assert!(lines.iter().any(|l| l == "playing coda · cycle 1/1"));
        assert_eq!(app.queued, 2, "the third link was queued after the swap");
        let snap = app.engine.clock();
        assert!(!snap.playing);
        assert_eq!((snap.segment, snap.frame), (2, 480_000 + 96_000));
        assert_eq!(app.active_block(), Some(2));
    }

    #[test]
    fn mute_toggled_on_the_queued_block_applies_at_the_swap() {
        let (mut app, mut cb, _tx) = app();
        app.handle(score_lesson(false));
        app.start(0).unwrap();
        cb.tick();
        let mark = app.engine.mark();
        app.toggle_mute(1, 1).unwrap();
        assert_eq!(app.engine.mark(), mark + 1, "one SetMute, no Queue");
        cb.tick();
        assert!(!cb.mixer().muted[1], "link 0 keeps its click");
        assert!(cb.mixer().next_muted[1]);
        while cb.mixer().segment == 0 {
            cb.tick();
            app.follow_score(Mono::now());
        }
        assert!(cb.mixer().muted[1]);
        assert_eq!(app.engine.mark(), mark + 1, "nothing re-queued");
        assert!(app.doc.as_ref().unwrap().blocks[1].muted[1]);
    }

    #[test]
    fn restart_of_a_score_block_plays_its_count_in_and_continues_the_score() {
        let (mut app, mut cb, _tx) = app();
        app.handle(score_lesson(true));
        app.start(0).unwrap();
        for _ in 0..100 {
            cb.tick();
        }
        let r = app.handle(Request::Play {
            label: Some("alone".into()),
        });
        assert!(r.ok, "{r:?}");
        let links: Vec<_> = app
            .chain
            .iter()
            .map(|l| (l.block, l.origin, l.tl.count_in_len))
            .collect();
        assert_eq!(links, [(1, 0, 96_000), (2, 288_000, 0)]);
        assert_eq!(app.active_block(), Some(1));
        tick_into_count_in(&app, &mut cb);
        assert_eq!(app.count_in_beat_now(), Some(1));
        let snap = app.engine.clock();
        assert_eq!((snap.segment, snap.origin), (0, 0));
    }

    fn tick_into_count_in(app: &App<()>, cb: &mut Callback) {
        for _ in 0..10 {
            if app.transport() == Transport::CountIn {
                return;
            }
            cb.tick();
        }
        panic!("never entered the count-in");
    }

    /// A clock for driving the callback: tick `k` starts at `t0 + k·TICK`,
    /// `latency` ahead of the speakers. A block played from tick 0 is heard
    /// at frame `h` at `wall(h)`.
    ///
    /// `t0` is unrelated to `Mono::now()`: a test driven by `Sim` asserts
    /// about a simulated instant (`transport_at(until)`, `report_at(until)`,
    /// `count_in_beat_at`, `link_now(at)`). Anything that resolves "now"
    /// from the wall clock — `transport()`, the `*_now()` helpers, a drawn
    /// frame's playhead and count-in, `Request::Report`'s choice among open
    /// performances — answers about the host's uptime, not the run. `t0`
    /// lies past any host's uptime, so such a read sees the same before-`t0`
    /// clock on every host; a `Sim` test uses one only for what does not
    /// depend on it (a frame's hit text and judged dots, a report with one
    /// performance open).
    struct Sim {
        t0: Mono,
        k: u64,
        latency: Duration,
    }

    impl Sim {
        fn new(latency_ms: u64) -> Sim {
            Sim {
                // ~31.7 years after boot, in nanoseconds.
                t0: Mono(1_000_000_000_000_000_000),
                k: 0,
                latency: Duration::from_millis(latency_ms),
            }
        }

        fn at(&self, secs: f64) -> Mono {
            self.t0 + Duration::from_secs_f64(secs)
        }

        fn wall(&self, heard: f64) -> Mono {
            self.at(self.latency.as_secs_f64() + heard / RATE)
        }

        /// Ticks the callback and judges after every tick until `until`;
        /// each stroke in `strokes` is sent once its time has come.
        fn run(
            &mut self,
            app: &mut App<()>,
            cb: &mut Callback,
            until: Mono,
            strokes: &mut Vec<(Mono, Part)>,
            tx: &mpsc::Sender<(Hit, Part)>,
        ) {
            loop {
                let t = self.at(self.k as f64 * TICK);
                if t > until {
                    break;
                }
                cb.tick_at(t, self.latency);
                self.k += 1;
                send_due(strokes, t, tx);
                app.follow_score(t);
                app.judge_hits(t);
            }
            send_due(strokes, until, tx);
            app.judge_hits(until);
        }
    }

    fn send_due(strokes: &mut Vec<(Mono, Part)>, now: Mono, tx: &mpsc::Sender<(Hit, Part)>) {
        strokes.retain(|(at, part)| {
            if *at > now {
                return true;
            }
            let hit = Hit {
                at: *at,
                trigger: key_code("f"),
                velocity: None,
            };
            tx.send((hit, part.clone())).unwrap();
            false
        });
    }

    fn part_f(app: &App<()>) -> Part {
        app.input.arming().unwrap().parts[&key_code("f")].clone()
    }

    fn scratch(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("djembot-ui-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    fn ended_by(app: &App<()>) -> Option<EndedBy> {
        app.last_run.as_ref().map(|r| r.ended_by)
    }

    #[test]
    fn a_performance_opens_from_the_count_in_and_not_on_resume_of_a_paused_block() {
        let (mut app, mut cb, _tx) = app();
        app.handle(perf_lesson("/l/p.md"));
        assert!(app.live.is_empty());
        app.handle(Request::Play {
            label: Some("N".into()),
        });
        assert!(app.live.is_empty(), "a block without a part opens nothing");

        app.start(1).unwrap();
        assert_eq!(app.live.last().map(|l| l.block), Some(1), "restart");
        assert_eq!(ended_by(&app), None, "nothing was open");
        cb.tick();
        app.engine.pause().unwrap();
        cb.tick();
        let mark = app.mark;
        app.resume(1).unwrap();
        assert_eq!(app.mark, mark, "resume continues it");
        assert_eq!(ended_by(&app), None);

        app.stop().unwrap();
        assert!(app.live.is_empty());
        assert_eq!(ended_by(&app), Some(EndedBy::Stop));
        cb.tick();
        app.resume(1).unwrap();
        assert!(!app.live.is_empty(), "resume of a stopped block");
        app.start(1).unwrap();
        assert_eq!(ended_by(&app), Some(EndedBy::Restart));
        app.resume(0).unwrap();
        assert_eq!(ended_by(&app), Some(EndedBy::Switch));
        assert_eq!(
            app.live.last().unwrap().block,
            0,
            "resume of a loaded block"
        );

        let Request::Show { path, lesson, .. } = perf_lesson("/l/p.md") else {
            unreachable!()
        };
        app.handle(Request::Show {
            path,
            lesson,
            play: Some("Q".into()),
        });
        assert_eq!(ended_by(&app), Some(EndedBy::Show));
        assert_eq!(app.armed, Some(1), "show --play arms a part block");
        assert!(app.live.is_empty(), "nothing is judged while armed");
        app.resume(1).unwrap();
        assert_eq!(app.live.last().unwrap().block, 1, "started from armed");
        assert_eq!(app.armed, None);
        app.handle(perf_lesson("/l/p.md"));
        assert_eq!(ended_by(&app), Some(EndedBy::Show));
        assert!(app.live.is_empty(), "show without play");
        app.resume(1).unwrap();
        app.quit();
        assert_eq!(ended_by(&app), Some(EndedBy::Quit));
    }

    #[test]
    fn report_returns_the_open_run_then_the_last_with_its_file() {
        let dir = scratch("report");
        let store = Store {
            runs: Some(dir.join("runs")),
            calibration: None,
        };
        let (mut app, mut cb, hit_tx, _tx) = app_with(store);
        assert_eq!(
            app.handle(Request::Report),
            Response::err("no performance yet")
        );
        app.handle(perf_lesson("/l/p.md"));
        app.resume(0).unwrap();
        let mut sim = Sim::new(0);
        sim.run(&mut app, &mut cb, sim.at(0.3), &mut vec![], &hit_tx);
        let open = app.handle(Request::Report);
        assert!(open.ok && open.labels.is_none() && open.run_path.is_none());
        let run = open.run.unwrap();
        assert_eq!((run.ended_by, run.label.as_str()), (EndedBy::Open, "P"));
        assert_eq!(run.misses.len(), 1, "pulse 0 missed so far");

        sim.run(&mut app, &mut cb, sim.at(2.6), &mut vec![], &hit_tx);
        let last = app.handle(Request::Report);
        let run = last.run.unwrap();
        // Pulse 0 in both cycles and the closing downbeat.
        assert_eq!((run.ended_by, run.misses.len()), (EndedBy::End, 3));
        let path = PathBuf::from(last.run_path.unwrap());
        assert_eq!(Some(&path), app.written.as_ref());
        assert!(path.exists());

        // A block without a part keeps the last run reportable.
        app.resume(2).unwrap();
        assert_eq!(app.handle(Request::Report).run.unwrap(), run);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_report_in_the_pass_a_block_closes_sees_the_closed_run() {
        let (mut app, mut cb, hit_tx, req_tx) = app_with(Store::default());
        app.handle(perf_lesson("/l/p.md"));
        app.resume(0).unwrap();
        let mut sim = Sim::new(0);
        sim.run(&mut app, &mut cb, sim.at(1.5), &mut vec![], &hit_tx);
        assert!(!app.live.is_empty());
        // The last stroke lands, the block ends and its last window closes,
        // with no pass in between; then a report arrives.
        let part = part_f(&app);
        send_due(&mut vec![(sim.at(1.95), part)], sim.at(1.95), &hit_tx);
        while sim.at(sim.k as f64 * TICK) <= sim.at(2.6) {
            cb.tick_at(sim.at(sim.k as f64 * TICK), sim.latency);
            sim.k += 1;
        }
        let (p, reply) = pending(Request::Report, Instant::now() + Duration::from_secs(5));
        req_tx.send(p).unwrap();
        let ctx = egui::Context::default();
        let _ = ctx.run_logic(&window_input(true, false), |ctx| app.tick(ctx, sim.at(2.6)));
        let run = reply.try_recv().unwrap().run.unwrap();
        assert_eq!(run.ended_by, EndedBy::End);
        // Pulse 0 and the closing downbeat were not struck.
        assert_eq!((run.hits.len(), run.misses.len()), (1, 2));
    }

    #[test]
    fn you_and_tally_texts() {
        let Request::Show { lesson, .. } = perf_lesson("/l/p.md") else {
            unreachable!()
        };
        let blocks: Vec<&Exercise> = lesson.blocks().collect();
        assert_eq!(you_text(blocks[0]).as_deref(), Some("you: clave (f)"));
        assert_eq!(you_text(blocks[2]), None);
        let ensemble = exercise(json!({
            "tempo": 100, "beats": 4,
            "voices": [{ "name": "conga", "instrument": "conga", "pulses": 8, "onsets": [0],
                         "keys": { "o": "f", "s": "g", "b": "v" } },
                       { "name": "bell", "instrument": "bell", "pulses": 8, "onsets": [0],
                         "keys": "d" },
                       { "name": "clave", "instrument": "clave", "pulses": 8, "onsets": [0] }]
        }));
        assert_eq!(
            you_text(&ensemble).as_deref(),
            Some("you: conga (f o · g s · v b), bell (d h)")
        );
        let tally = Tally {
            hit: 24,
            miss: 2,
            extra: 1,
            mean_ms: Some(8.4),
        };
        assert_eq!(tally_text(tally), "hit 24 · miss 2 · extra 1 · mean +8 ms");
        let none = Tally {
            hit: 0,
            mean_ms: None,
            ..tally
        };
        assert_eq!(tally_text(none), "hit 0 · miss 2 · extra 1");
    }

    #[test]
    fn marks_of_earlier_cycles_fade_and_older_ones_go() {
        let ex = exercise(json!({
            "tempo": 120, "beats": 4,
            "voices": [{ "name": "clave", "instrument": "clave", "pulses": 4,
                         "onsets": [0], "keys": "f" }]
        }));
        let mut bank = Bank::from_fn(|_| Arc::from(vec![1.0f32]));
        let tl = Timeline::from(&ex, &mut bank, 48_000).unwrap();
        let mut p = Performance::new(
            &ex,
            &tl,
            0,
            Default::default(),
            48_000,
            perf::conditions("/l/x.md", 0.0, None),
        );
        let cycle = |k: u32| k as f64 * 96_000.0;
        for k in [0, 7, 8, 39, 40] {
            p.judge(cycle(k), 0, 'x', None);
        }
        p.advance(cycle(40) + 1.0);
        p.judge(cycle(41) - 480.0, 0, 'x', None); // early, into the next cycle
        let hits: Vec<(i64, f32)> = visible_marks(&p)
            .iter()
            .filter(|(m, _)| matches!(m.kind, MarkKind::Hit(_)))
            .map(|(m, a)| (m.cycle, *a))
            .collect();
        assert_eq!(
            hits,
            vec![(8, PAST_ALPHA), (39, PAST_ALPHA), (40, 1.0), (41, 1.0)]
        );
        let misses = visible_marks(&p)
            .iter()
            .filter(|(m, _)| m.kind == MarkKind::Miss)
            .count();
        assert_eq!(misses, 30, "cycles 9..=38 missed, older ones not drawn");
    }

    /// Filled circles drawn in `color`.
    fn dots(out: &egui::FullOutput, color: Color32) -> usize {
        out.shapes
            .iter()
            .filter(|c| matches!(&c.shape, egui::Shape::Circle(s) if s.fill == color))
            .count()
    }

    #[test]
    fn the_header_and_marks_show_the_performance_until_the_next_one() {
        let (mut app, mut cb, hit_tx, _tx) = app_with(Store::default());
        let ctx = egui::Context::default();
        app.handle(perf_lesson("/l/p.md"));
        let out = press(&mut app, &ctx, &[]);
        assert_eq!(texts(&out, "you: clave (f)"), 2, "blocks P and Q");
        assert_eq!(dots(&out, ON_COLOR), 0);

        app.resume(1).unwrap();
        let mut sim = Sim::new(0);
        let mut strokes = vec![(sim.at(0.010), part_f(&app))];
        sim.run(&mut app, &mut cb, sim.at(0.3), &mut strokes, &hit_tx);
        let live = "hit 1 · miss 0 · extra 0 · mean +10 ms";
        let out = press(&mut app, &ctx, &[]);
        assert_eq!(texts(&out, live), 1, "only on the played block");
        assert_eq!(dots(&out, ON_COLOR), 2, "grid and necklace");

        app.stop().unwrap();
        let out = press(&mut app, &ctx, &[]);
        assert_eq!(texts(&out, live), 1, "kept after the close");
        assert_eq!(dots(&out, ON_COLOR), 2);
        press(&mut app, &ctx, &[Key::H]);
        let out = press(&mut app, &ctx, &[]);
        assert_eq!(texts(&out, live), 1, "a hidden block keeps the count");
        assert_eq!(dots(&out, ON_COLOR), 0);
        press(&mut app, &ctx, &[Key::H]);

        app.resume(0).unwrap();
        let out = press(&mut app, &ctx, &[]);
        assert_eq!(texts(&out, live), 0, "a new performance replaces it");
        assert_eq!(texts(&out, "hit 0 · miss 0 · extra 0"), 1);
        app.stop().unwrap();
        app.handle(perf_lesson("/l/p.md"));
        let out = press(&mut app, &ctx, &[]);
        assert_eq!(texts(&out, "hit 0 · miss 0 · extra 0"), 0, "show clears it");
    }

    /// Circles drawn as a ring (no fill) in `color`.
    fn rings(out: &egui::FullOutput, color: Color32) -> usize {
        out.shapes
            .iter()
            .filter(|c| {
                matches!(&c.shape, egui::Shape::Circle(s)
                    if s.fill == Color32::TRANSPARENT && s.stroke.color == color)
            })
            .count()
    }

    /// Line segments drawn in `color`.
    fn segments(out: &egui::FullOutput, color: Color32) -> usize {
        out.shapes
            .iter()
            .filter(|c| matches!(&c.shape, egui::Shape::LineSegment { stroke, .. } if stroke.color == color))
            .count()
    }

    /// The centres of the filled circles drawn in `color`.
    fn dot_centers(out: &egui::FullOutput, color: Color32) -> Vec<Pos2> {
        out.shapes
            .iter()
            .filter_map(|c| match &c.shape {
                egui::Shape::Circle(s) if s.fill == color => Some(s.center),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn every_kind_of_mark_is_drawn_on_grid_and_necklace_in_its_color() {
        let (mut app, mut cb, hit_tx, _tx) = app_with(Store::default());
        let ctx = egui::Context::default();
        let ex = exercise(json!({
            "label": "M", "tempo": 120, "beats": 4, "window_ms": 100,
            "voices": [{ "name": "clave", "instrument": "clave", "pulses": 16,
                         "onsets": [0, 4, 8, 12], "keys": "f" }]
        }));
        app.handle(Request::Show {
            path: "/l/m.md".into(),
            lesson: Lesson {
                segments: vec![Segment::Rhythm(ex)],
            },
            play: None,
        });
        app.resume(0).unwrap();
        let mut sim = Sim::new(0);
        let f = part_f(&app);
        // Pulse 0 missed, 4 on time, 8 late, 12 missed, an extra between
        // them, and pulse 0 of the next cycle 50 ms early.
        let mut strokes = vec![
            (sim.at(0.505), f.clone()),
            (sim.at(1.035), f.clone()),
            (sim.at(1.25), f.clone()),
            (sim.at(1.95), f.clone()),
        ];
        sim.run(&mut app, &mut cb, sim.at(1.96), &mut strokes, &hit_tx);
        let out = press(&mut app, &ctx, &[]);
        assert_eq!(dots(&out, ON_COLOR), 2);
        assert_eq!(dots(&out, LATE_COLOR), 2);
        assert_eq!(dots(&out, EARLY_COLOR), 2);
        assert_eq!(rings(&out, WRONG_COLOR), 4, "two misses");
        assert_eq!(segments(&out, WRONG_COLOR), 4, "one extra's cross");

        // On the grid the on-time and late dots share their row; from them,
        // where the row starts. The early pulse-0 dot is held at the row's
        // start, not drawn as far left as its time would put it.
        let on = dot_centers(&out, ON_COLOR);
        let late = dot_centers(&out, LATE_COLOR);
        let (on, late) = on
            .iter()
            .flat_map(|a| late.iter().map(move |b| (*a, *b)))
            .find(|(a, b)| (a.y - b.y).abs() < 0.01)
            .expect("a grid row holding both dots");
        let width = (late.x - on.x) / (0.5175 - 0.2525);
        let left = on.x - width * 0.2525;
        let early = dot_centers(&out, EARLY_COLOR)
            .into_iter()
            .find(|p| (p.y - on.y).abs() < 0.01)
            .expect("the early dot on the grid row");
        assert!(left - width * 0.025 < early.x - 1.0, "clamped: {early:?}");
        assert!(
            early.x >= left - 9.5 && early.x <= left,
            "{early:?} vs {left}"
        );
    }

    /// Calls `serve(now)` on another thread, as the IPC connection thread
    /// does; this only compiles while `App` is `Send`.
    fn serve_elsewhere(app: &Arc<Mutex<App<()>>>, now: Mono) {
        let app = app.clone();
        std::thread::spawn(move || lock(&app).serve(now))
            .join()
            .unwrap();
    }

    #[test]
    fn a_request_is_answered_without_a_pass() {
        let dir = scratch("serve");
        let store = Store {
            runs: Some(dir.join("runs")),
            calibration: None,
        };
        let (mut app, mut cb, hit_tx, req_tx) = app_with(store);
        app.handle(perf_lesson("/l/p.md"));
        app.resume(1).unwrap();
        let mut sim = Sim::new(0);
        sim.run(&mut app, &mut cb, sim.at(1.9), &mut vec![], &hit_tx);
        let part = part_f(&app);
        let app = Arc::new(Mutex::new(app));
        // The audio runs on, a stroke lands on cycle 1's pulse 0, and a
        // report arrives, with no pass in between.
        while sim.at(sim.k as f64 * TICK) <= sim.at(2.05) {
            cb.tick_at(sim.at(sim.k as f64 * TICK), sim.latency);
            sim.k += 1;
        }
        send_due(&mut vec![(sim.at(2.01), part)], sim.at(2.05), &hit_tx);
        let deadline = Instant::now() + Duration::from_secs(5);
        let (p, reply) = pending(Request::Report, deadline);
        req_tx.send(p).unwrap();
        serve_elsewhere(&app, sim.at(2.05));
        let run = reply.try_recv().unwrap().run.unwrap();
        assert_eq!(run.ended_by, EndedBy::Open);
        let hit = run
            .hits
            .iter()
            .find(|h| h.judgment == perf::RunJudgment::Hit)
            .expect("the stroke is judged");
        assert_eq!((hit.cycle, hit.pulse), (1, Some(0)));

        let (p, stop_reply) = pending(Request::Stop, deadline);
        req_tx.send(p).unwrap();
        let (p, report_reply) = pending(Request::Report, deadline);
        req_tx.send(p).unwrap();
        serve_elsewhere(&app, sim.at(2.06));
        assert!(stop_reply.try_recv().unwrap().ok);
        let last = report_reply.try_recv().unwrap();
        let closed = last.run.unwrap();
        assert_eq!(closed.ended_by, EndedBy::Stop);
        assert_eq!(closed.hits, run.hits);
        assert!(PathBuf::from(last.run_path.unwrap()).exists());
        assert!(lock(&app).live.is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_dead_stream_closes_the_performance_without_inventing_misses() {
        let (mut app, mut cb, hit_tx, _tx) = app_with(Store::default());
        app.handle(perf_lesson("/l/p.md"));
        app.resume(1).unwrap();
        let mut sim = Sim::new(0);
        sim.run(&mut app, &mut cb, sim.at(0.3), &mut vec![], &hit_tx);
        assert_eq!(app.live.last().unwrap().perf.misses().len(), 1);
        app.engine.kill("audio stream error: gone");
        app.judge_hits(sim.at(60.0));
        assert!(app.live.is_empty());
        let run = app.last_run.as_ref().unwrap();
        assert_eq!((run.ended_by, run.misses.len()), (EndedBy::Stop, 1));
    }

    #[test]
    fn a_dead_stream_ends_a_block_without_a_part() {
        let (mut app, mut cb, hit_tx, _tx) = app_with(Store::default());
        app.handle(score_lesson(false));
        let rx = subscribe(&mut app);
        app.handle(Request::Play {
            label: Some("alone".into()),
        });
        let mut sim = Sim::new(0);
        sim.run(&mut app, &mut cb, sim.at(0.3), &mut vec![], &hit_tx);
        app.engine.kill("audio stream error: gone");
        app.judge_hits(sim.at(60.0));
        app.judge_hits(sim.at(61.0));
        assert_eq!(
            lines(&rx),
            [
                "idle",
                "started label=alone",
                "ended label=alone by=stop cycles=0/2 !",
            ],
            "ended once, terminal"
        );
    }

    #[test]
    fn show_warns_when_a_part_has_no_readable_keyboard() {
        const WARNING: &str = "no keyboard device is readable; parts with \"keys\" will record \
                               only misses — see README \"Keyboard access\"";
        let (mut app, _cb, _tx) = app();
        assert_eq!(app.handle(perf_lesson("/l/p.md")).warnings, [WARNING]);
        *app.input.status.lock().unwrap() =
            input::InputStatus::NoAccess("/dev/input/event0".into());
        assert_eq!(app.handle(Request::Calibrate).warnings, [WARNING]);
        assert!(
            app.handle(Request::Play { label: None })
                .warnings
                .is_empty()
        );

        *app.input.status.lock().unwrap() = input::InputStatus::Devices(1);
        let r = app.handle(perf_lesson("/l/p.md"));
        assert!(r.ok && r.warnings.is_empty(), "{r:?}");

        *app.input.status.lock().unwrap() = input::InputStatus::None;
        let Request::Show {
            path, mut lesson, ..
        } = perf_lesson("/l/n.md")
        else {
            unreachable!()
        };
        lesson.segments.drain(..2);
        let r = app.handle(Request::Show {
            path,
            lesson,
            play: None,
        });
        assert_eq!(r, Response::ok(vec!["N".into()]), "no part, no warning");
    }

    /// A listen-only `lead` of one cycle, attacca into `part`, two cycles
    /// with a clave on `f`; then `free`, listen-only.
    fn lead_lesson() -> Request {
        let listen = |label: &str, next: &str| {
            exercise(json!({
                "label": label, "tempo": 120, "beats": 4, "cycles": 1, "count_in": 4,
                "next": next,
                "voices": [{ "name": "click", "instrument": "click", "pulses": 4,
                             "onsets": [0, 1, 2, 3] }]
            }))
        };
        Request::Show {
            path: "/l/lead.md".into(),
            lesson: Lesson {
                segments: vec![
                    Segment::Rhythm(listen("lead", "attacca")),
                    Segment::Rhythm(exercise(json!({
                        "label": "part", "tempo": 120, "beats": 4, "cycles": 2,
                        "voices": [{ "name": "clave", "instrument": "clave", "pulses": 16,
                                     "onsets": [0, 6], "keys": "f" }]
                    }))),
                    Segment::Rhythm(listen("free", "stop")),
                ],
            },
            play: None,
        }
    }

    #[test]
    fn play_of_a_part_block_arms_it_and_space_starts_it() {
        let (mut app, mut cb, _tx) = app();
        let ctx = egui::Context::default();
        app.handle(perf_lesson("/l/p.md"));
        app.start(2).unwrap();
        cb.tick();
        let r = app.handle(Request::Play {
            label: Some("P".into()),
        });
        assert!(r.ok, "{r:?}");
        assert_eq!(r.armed.as_deref(), Some("P"));
        assert_eq!(app.armed, Some(0));
        assert!(app.live.is_empty(), "nothing is judged while armed");
        assert_eq!(ended_by(&app), None, "N had no performance");
        cb.tick();
        assert!(!app.engine.clock().playing, "the playing block stopped");
        assert!(armed_keys(&app).is_some(), "the part's keys sound");
        assert_eq!(app.transport(), Transport::Armed);
        let doc = app.doc.as_ref().unwrap();
        assert_eq!(app.transport_line(doc, Mono::now()), "armed P");
        assert_eq!((doc.active, doc.selected), (Some(0), 0));
        let out = press(&mut app, &ctx, &[]);
        assert_eq!(texts(&out, ARMED_TEXT), 1, "on the armed block only");

        press(&mut app, &ctx, &[Key::J]);
        press(&mut app, &ctx, &[Key::K]);
        assert_eq!(app.armed, Some(0), "moving the selection keeps it armed");
        press(&mut app, &ctx, &[Key::Space]);
        assert_eq!(app.armed, None);
        assert_eq!(app.live.last().map(|l| l.block), Some(0));
        cb.tick();
        assert!(app.playing());
        let out = press(&mut app, &ctx, &[]);
        assert_eq!(texts(&out, ARMED_TEXT), 0);

        // Arming over a performance stops it.
        let r = app.handle(Request::Play {
            label: Some("Q".into()),
        });
        assert_eq!(r.armed.as_deref(), Some("Q"));
        assert_eq!(ended_by(&app), Some(EndedBy::Stop));
        assert!(app.live.is_empty());
        // `stop` disarms; `r` starts an armed block.
        app.handle(Request::Stop);
        assert_eq!(app.armed, None);
        app.handle(Request::Play {
            label: Some("Q".into()),
        });
        let doc = app.doc.as_mut().unwrap();
        doc.selected = 1;
        press(&mut app, &ctx, &[Key::R]);
        assert_eq!(
            (app.armed, app.live.last().map(|l| l.block)),
            (None, Some(1))
        );
    }

    #[test]
    fn a_listen_only_head_with_a_part_later_in_its_score_is_armed() {
        let (mut app, mut cb, _tx) = app();
        let ctx = egui::Context::default();
        let Request::Show { path, lesson, .. } = lead_lesson() else {
            unreachable!()
        };
        let r = app.handle(Request::Show {
            path,
            lesson,
            play: Some("lead".into()),
        });
        assert!(r.ok, "{r:?}");
        assert_eq!(r.armed.as_deref(), Some("lead"), "the head is armed");
        assert_eq!(app.armed, Some(0));
        assert_eq!(app.chain.len(), 2, "the whole score is loaded");
        cb.tick();
        assert!(!app.engine.clock().playing);
        assert_eq!(app.transport(), Transport::Armed);
        let out = press(&mut app, &ctx, &[]);
        assert_eq!(texts(&out, ARMED_TEXT), 1);
        assert_eq!(texts(&out, "free"), 1);

        press(&mut app, &ctx, &[Key::Space]);
        assert_eq!(app.armed, None);
        tick_into_count_in(&app, &mut cb);
        assert_eq!(app.count_in_beat_now(), Some(1), "the head's count-in");
        assert_eq!(app.chain.len(), 2);
    }

    #[test]
    fn play_of_a_listen_only_score_starts_it() {
        let (mut app, mut cb, _tx) = app();
        app.handle(score_lesson(true));
        let r = app.handle(Request::Play {
            label: Some("guided".into()),
        });
        assert!(r.ok && r.armed.is_none(), "{r:?}");
        assert_eq!(app.armed, None);
        cb.tick();
        assert!(app.playing(), "a score of three listen-only links");
        let Request::Show { path, lesson, .. } = lead_lesson() else {
            unreachable!()
        };
        let r = app.handle(Request::Show {
            path,
            lesson,
            play: Some("free".into()),
        });
        assert!(r.ok && r.armed.is_none(), "{r:?}");
        cb.tick();
        assert!(app.playing(), "a listen-only block after the part");
    }

    /// The playhead line and hand, and the halos of the strokes it passes,
    /// are drawn only on a block that keeps its playhead; marks stay.
    #[test]
    fn playhead_false_draws_no_playhead_hand_or_flash() {
        let draw = |playhead: bool| {
            let (mut app, mut cb, hit_tx, _tx) = app_with(Store::default());
            let ctx = egui::Context::default();
            let ex = exercise(json!({
                "label": "M", "tempo": 60, "beats": 4, "playhead": playhead,
                "voices": [{ "name": "clave", "instrument": "clave", "pulses": 4,
                             "onsets": [0, 1, 2, 3], "keys": "f", "mute": false }]
            }));
            app.handle(Request::Show {
                path: "/l/m.md".into(),
                lesson: Lesson {
                    segments: vec![Segment::Rhythm(ex)],
                },
                play: None,
            });
            app.resume(0).unwrap();
            let f = part_f(&app);
            let mut sim = Sim::new(0);
            let mut strokes = vec![(sim.at(0.010), f)];
            sim.run(&mut app, &mut cb, sim.at(0.05), &mut strokes, &hit_tx);
            cb.tick();
            let out = press(&mut app, &ctx, &[]);
            let text = ctx.global_style().visuals.text_color();
            let circles = out
                .shapes
                .iter()
                .filter(|c| matches!(c.shape, egui::Shape::Circle(_)))
                .count();
            (segments(&out, text), circles, dots(&out, ON_COLOR))
        };
        let (lines, circles, marks) = draw(true);
        assert_eq!((lines, marks), (2, 2), "playhead line and hand; marks");
        let (lines_off, circles_off, marks_off) = draw(false);
        assert_eq!((lines_off, marks_off), (0, 2), "no playhead; marks kept");
        assert_eq!(circles - circles_off, 2, "the lit pulse-0 halos");
    }

    #[test]
    fn calibrate_shows_and_plays_the_calibration_block() {
        let (mut app, _cb, _tx) = app();
        let r = app.handle(Request::Calibrate);
        assert!(r.ok, "{r:?}");
        assert!(r.armed.is_none() && app.armed.is_none(), "starts at once");
        assert_eq!(r.labels, Some(vec!["cal".to_string()]));
        assert_eq!(app.doc.as_ref().unwrap().path, perf::CALIBRATION_NAME);
        let live = app.live.last().unwrap();
        assert!(live.perf.is_calibration() && live.block == 0);
        let tl = &app.chain[0].tl;
        assert_eq!((tl.count_in_len, tl.end), (0, None), "no count-in, no end");
        assert!(part_f(&app).buf.is_none(), "taps make no sound");
    }

    #[test]
    fn resuming_a_finished_block_opens_a_new_performance() {
        let (mut app, mut cb, hit_tx, _tx) = app_with(Store::default());
        app.handle(perf_lesson("/l/p.md"));
        app.resume(0).unwrap();
        let mut sim = Sim::new(0);
        sim.run(&mut app, &mut cb, sim.at(2.6), &mut vec![], &hit_tx);
        assert!(app.live.is_empty());
        assert_eq!(ended_by(&app), Some(EndedBy::End));
        app.resume(0).unwrap();
        let live = app.live.last().unwrap();
        assert_eq!(
            live.last_playing, None,
            "start clears the retained snapshot"
        );
        assert!(live.perf.misses().is_empty());
    }

    /// A late last stroke: 200 ms output latency, the last
    /// target 125 ms before the end, a stroke `window/2` after it that
    /// arrives only once the mixer has stopped.
    #[test]
    fn the_closing_downbeat_tap_is_a_hit() {
        let (mut app, mut cb, hit_tx, _tx) = app_with(Store::default());
        app.handle(perf_lesson("/l/p.md"));
        app.resume(0).unwrap();
        let part = part_f(&app);
        let mut sim = Sim::new(200);
        // One cycle of 96 000 frames: the "one" after it, 10 ms early.
        let mut strokes = vec![(sim.wall(96_000.0 - 480.0), part)];
        sim.run(
            &mut app,
            &mut cb,
            sim.wall(120_100.0),
            &mut strokes,
            &hit_tx,
        );
        assert!(app.live.is_empty(), "closed past end + beat");
        let run = app.last_run.as_ref().unwrap();
        assert_eq!(
            (run.ended_by, run.ending),
            (EndedBy::End, crate::model::Ending::Downbeat)
        );
        assert_eq!(run.hits.len(), 1);
        let h = &run.hits[0];
        assert_eq!(
            (h.judgment, h.cycle, h.pulse),
            (perf::RunJudgment::Hit, 1, Some(0))
        );
        assert!((h.error_ms.unwrap() + 10.0).abs() < 0.1, "{h:?}");
        // Pulses 0 and 15 of the one cycle were not struck.
        assert_eq!(run.misses.len(), 2);
    }

    /// A two-block score at 120 bpm, 2 cycles each (96 000 frames a cycle
    /// at 48 kHz): `guided`, with a 4-beat count-in and the clave on
    /// `onsets_a` struck on `f`, then attacca `alone`, the clave on 0 6 15
    /// struck on `key_b`. `guided` ends, and `alone` begins, at frame
    /// 288 000.
    fn part_score(onsets_a: &[u32], key_b: &str) -> Request {
        part_score_with(onsets_a, &[0, 6, 15], key_b)
    }

    /// `part_score` with `alone`'s clave on `onsets_b`.
    fn part_score_with(onsets_a: &[u32], onsets_b: &[u32], key_b: &str) -> Request {
        let block = |label: &str, count_in: u32, onsets: &[u32], key: &str, next: &str| {
            exercise(json!({
                "label": label, "tempo": 120, "beats": 4, "cycles": 2, "count_in": count_in,
                "next": next,
                "voices": [{ "name": "clave", "instrument": "clave", "pulses": 16,
                             "onsets": onsets, "keys": key, "mute": false }]
            }))
        };
        Request::Show {
            path: "/l/score.md".into(),
            lesson: Lesson {
                segments: vec![
                    Segment::Rhythm(block("guided", 4, onsets_a, "f", "attacca")),
                    Segment::Rhythm(block("alone", 0, onsets_b, key_b, "stop")),
                ],
            },
            play: None,
        }
    }

    const ORIGIN_B: f64 = 288_000.0;
    /// The `Sim` latency of `play_boundary`.
    const BOUNDARY_LATENCY_MS: u64 = 20;

    /// The open or closed performance of `block`.
    fn perf_of(app: &App<()>, block: usize) -> Option<&Performance> {
        let live = app.live.iter().map(|l| (l.block, &l.perf));
        let closed = app.closed.iter().map(|(b, p)| (*b, p));
        live.chain(closed)
            .find(|(b, _)| *b == block)
            .map(|(_, p)| p)
    }

    fn hits_of(app: &App<()>, block: usize) -> Vec<perf::HitRecord> {
        perf_of(app, block)
            .map(|p| p.hits().to_vec())
            .unwrap_or_default()
    }

    /// Plays `part_score_with(onsets_a, onsets_b, "f")` with a stroke at
    /// each heard frame of `strokes` until heard `until`.
    fn play_boundary(onsets_a: &[u32], onsets_b: &[u32], strokes: &[f64], until: f64) -> TestApp {
        let (mut app, mut cb, hit_tx, tx) = app_with(Store::default());
        app.handle(part_score_with(onsets_a, onsets_b, "f"));
        app.start(0).unwrap();
        let part = part_f(&app);
        let mut sim = Sim::new(BOUNDARY_LATENCY_MS);
        let mut strokes = strokes
            .iter()
            .map(|&f| (sim.wall(f), part.clone()))
            .collect();
        sim.run(&mut app, &mut cb, sim.wall(until), &mut strokes, &hit_tx);
        (app, cb, hit_tx, tx)
    }

    /// Plays `part_score(&[0, 6, 15], "f")` with a stroke at each heard
    /// frame of `strokes` and returns the judgments `guided` and `alone`
    /// recorded, as `(cycle, target, error)` for a hit and `None` for an
    /// extra.
    fn boundary_judgments(strokes: &[f64]) -> [Vec<Option<(u32, usize, f64)>>; 2] {
        let until = ORIGIN_B + 96_000.0;
        let (app, ..) = play_boundary(&[0, 6, 15], &[0, 6, 15], strokes, until);
        assert_eq!(app.live.len(), 1, "guided closed after its judged span");
        let run = app.last_run.as_ref().unwrap();
        assert_eq!((run.label.as_str(), run.ended_by), ("guided", EndedBy::End));
        let report = app.report_at(Sim::new(BOUNDARY_LATENCY_MS).wall(until));
        assert_eq!(report.run.as_ref().map(|r| r.label.as_str()), Some("alone"));
        [0, 1].map(|block| {
            hits_of(&app, block)
                .iter()
                .map(|h| match h.judgment {
                    perf::Judgment::Hit {
                        cycle,
                        target,
                        error,
                    } => Some((cycle, target, error.round())),
                    _ => None,
                })
                .collect()
        })
    }

    /// `(cycle, pulse, error_ms, judgment)` of the strokes `block` recorded.
    fn annotations_of(
        app: &App<()>,
        block: usize,
    ) -> Vec<(i64, Option<u32>, Option<f64>, perf::RunJudgment)> {
        perf_of(app, block).map_or_else(Vec::new, |p| {
            p.run(EndedBy::Open)
                .hits
                .iter()
                .map(|h| (h.cycle, h.pulse, h.error_ms.map(f64::round), h.judgment))
                .collect()
        })
    }

    #[test]
    fn a_stroke_near_the_boundary_goes_to_the_nearer_target() {
        // `guided`'s last target, pulse 15 of cycle 1, is 6 000 frames
        // (125 ms) before `alone`'s first; their 100 ms windows overlap.
        let last_a = ORIGIN_B - 6_000.0;
        // Early for B's downbeat (2 500), nearer than A's last (3 500).
        let [a, b] = boundary_judgments(&[ORIGIN_B - 2_500.0]);
        assert_eq!((a, b), (vec![], vec![Some((0, 0, -2_500.0))]));
        // Late for A's last (2 000), nearer than B's downbeat (4 000).
        let [a, b] = boundary_judgments(&[last_a + 2_000.0]);
        assert_eq!((a, b), (vec![Some((1, 2, 2_000.0))], vec![]));
        // A claimed target passes the stroke to the other block; with both
        // claimed, the block whose real target is nearer records the extra.
        let [a, b] = boundary_judgments(&[
            last_a + 1_000.0,
            ORIGIN_B - 2_000.0,
            ORIGIN_B - 1_000.0,
            ORIGIN_B + 1_000.0,
        ]);
        assert_eq!(a, vec![Some((1, 2, 1_000.0))]);
        assert_eq!(b, vec![Some((0, 0, -2_000.0)), None, None]);
        // A's last target far from the boundary: a stroke 150 ms early for
        // B's first target is B's extra, annotated against it.
        let (app, ..) = play_boundary(
            &[0, 6],
            &[0, 6, 15],
            &[ORIGIN_B - 7_200.0],
            ORIGIN_B + 96_000.0,
        );
        assert_eq!(annotations_of(&app, 0), []);
        assert_eq!(
            annotations_of(&app, 1),
            [(0, Some(0), Some(-150.0), perf::RunJudgment::Extra)]
        );
        // A's last target one pulse before the boundary, B's first four
        // after it: a stroke 250 ms late for A's last target, past A's
        // window but in its judged span, is A's extra.
        let (app, ..) = play_boundary(
            &[0, 15],
            &[4, 10],
            &[ORIGIN_B + 6_000.0],
            ORIGIN_B + 96_000.0,
        );
        assert_eq!(
            annotations_of(&app, 0),
            [(1, Some(15), Some(250.0), perf::RunJudgment::Extra)]
        );
        assert_eq!(annotations_of(&app, 1), []);
    }

    #[test]
    fn a_boundary_rush_is_wide_in_the_next_blocks_report() {
        // B ends at ORIGIN_B + 192 000; it closes a beat after that.
        let (app, ..) = play_boundary(
            &[0, 6],
            &[0, 6, 15],
            &[ORIGIN_B - 7_200.0],
            ORIGIN_B + 220_000.0,
        );
        assert!(app.live.is_empty(), "both blocks closed");
        let overall = |block: usize| {
            let run = perf_of(&app, block).unwrap().run(EndedBy::End);
            let text = crate::report::render(&run, None);
            // targets hit miss extra wide
            crate::report::tests::row_of(&text, "Overall", "all")[1..6]
                .iter()
                .map(|c| c.to_string())
                .collect::<Vec<_>>()
        };
        assert_eq!(overall(0), ["4", "0", "4", "0", "0"]);
        assert_eq!(overall(1), ["7", "0", "7", "1", "1"]);
    }

    #[test]
    fn next_performance_opens_on_the_judged_clock() {
        let (mut app, mut cb, hit_tx, _tx) = app_with(Store::default());
        app.calibration = Some(Calibration {
            offset_ms: -45.0,
            sd_ms: 0.0,
            taps: 24,
            measured: String::new(),
        });
        // `guided`'s last target is far from the boundary; `alone` is on `d`.
        app.handle(part_score(&[0, 6], "d"));
        app.start(0).unwrap();
        assert_eq!(armed_keys(&app), Some(vec![key_code("f")]));
        let mut sim = Sim::new(20);
        // B's judged span opens a beat (24 000 frames) before its origin on
        // the judged clock (heard + 45 ms), at heard ORIGIN_B − 24 000 −
        // 2 160. The stroke on `d` is 240 frames later, judged 495 ms
        // before B's first target. A does not bind `d`: had B opened on the
        // heard clock, nothing would receive the stroke.
        let threshold = ORIGIN_B - 24_000.0 - 2_160.0;
        let stroke = threshold + 240.0;
        sim.run(
            &mut app,
            &mut cb,
            sim.wall(threshold - 300.0),
            &mut Vec::new(),
            &hit_tx,
        );
        assert_eq!(app.live.len(), 1, "B is not open before its span");
        sim.run(
            &mut app,
            &mut cb,
            sim.wall(threshold),
            &mut Vec::new(),
            &hit_tx,
        );
        assert_eq!(app.live.len(), 2, "B opened with its span");
        assert_eq!(
            armed_keys(&app),
            Some(vec![key_code("d"), key_code("f")]),
            "both blocks' keys sound across the boundary"
        );
        let part = app.input.arming().unwrap().parts[&key_code("d")].clone();
        hit_tx
            .send((
                Hit {
                    at: sim.wall(stroke),
                    trigger: key_code("d"),
                    velocity: None,
                },
                part,
            ))
            .unwrap();
        sim.run(
            &mut app,
            &mut cb,
            sim.wall(ORIGIN_B + 10_000.0),
            &mut Vec::new(),
            &hit_tx,
        );
        assert!(hits_of(&app, 0).is_empty());
        let b = hits_of(&app, 1);
        assert_eq!(b.len(), 1);
        assert_eq!(b[0].judgment, perf::Judgment::Extra, "{b:?}");
        let h = &app.live[1].perf.run(EndedBy::Open).hits[0];
        assert_eq!(
            (h.t_ms.round(), h.cycle, h.pulse, h.error_ms.map(f64::round)),
            (-495.0, 0, Some(0), Some(-495.0))
        );
        // A closes a beat after its end on the judged clock: after heard
        // ORIGIN_B + 21 840.
        assert_eq!(app.live.len(), 2, "A still open");
        assert_eq!(armed_keys(&app), Some(vec![key_code("d"), key_code("f")]));
        sim.run(
            &mut app,
            &mut cb,
            sim.wall(ORIGIN_B + 30_000.0),
            &mut Vec::new(),
            &hit_tx,
        );
        assert_eq!(app.live.len(), 1, "A closed");
        assert_eq!(armed_keys(&app), Some(vec![key_code("d")]));
        let run = app.live[0].perf.run(EndedBy::Open);
        assert_eq!((run.label.as_str(), run.count_in), ("alone", 0));
    }

    #[test]
    fn the_sounding_block_wins_a_key_bound_in_two_links() {
        let block = |label: &str, tone: &str, gain: f64, next: &str| {
            exercise(json!({
                "label": label, "tempo": 120, "beats": 4, "cycles": 2, "next": next,
                "voices": [{ "name": "bell", "instrument": "bell", "pulses": 4,
                             "onsets": [0], "keys": { tone: "f" }, "gain": gain }]
            }))
        };
        let (mut app, mut cb, hit_tx, _tx) = app_with(Store::default());
        app.handle(Request::Show {
            path: "/l/score.md".into(),
            lesson: Lesson {
                segments: vec![
                    Segment::Rhythm(block("A", "h", 0.5, "attacca")),
                    Segment::Rhythm(block("B", "l", 0.25, "stop")),
                ],
            },
            play: None,
        });
        app.start(0).unwrap();
        // No count-in: B begins after A's two cycles.
        let origin_b = 192_000.0;
        let gain = |app: &App<()>| app.input.arming().unwrap().parts[&key_code("f")].gain;
        let mut sim = Sim::new(20);
        let mut run_to = |app: &mut App<()>, heard: f64| {
            let until = sim.wall(heard);
            sim.run(app, &mut cb, until, &mut Vec::new(), &hit_tx);
        };
        run_to(&mut app, origin_b - 1_000.0);
        assert_eq!(app.live.len(), 2, "B open a beat early");
        assert_eq!(gain(&app), 0.5, "A sounds");
        run_to(&mut app, origin_b + 1_000.0);
        assert_eq!(app.live.len(), 2, "A open a beat late");
        assert_eq!(gain(&app), 0.25, "B sounds");
        run_to(&mut app, origin_b + 30_000.0);
        assert_eq!(app.live.len(), 1, "A closed");
        assert_eq!(gain(&app), 0.25);
    }

    #[test]
    fn a_late_last_stroke_is_judged_from_the_last_playing_snapshot() {
        let dir = scratch("late");
        let store = Store {
            runs: Some(dir.join("runs")),
            calibration: None,
        };
        let (mut app, mut cb, hit_tx, _tx) = app_with(store);
        app.handle(perf_lesson("/l/p.md"));
        app.resume(0).unwrap();
        let part = part_f(&app);
        let mut sim = Sim::new(200);
        let stroke = sim.wall(90_000.0 + 2_400.0);
        // Run to just before the stroke: the mixer has stopped by then.
        sim.run(
            &mut app,
            &mut cb,
            stroke - Duration::from_millis(1),
            &mut vec![],
            &hit_tx,
        );
        let snap = app.engine.since(app.mark).unwrap();
        assert!(!snap.playing && snap.frame == 96_000, "finished: {snap:?}");
        assert!(app.post_end() && app.animating(), "still repainting");
        let mut strokes = vec![(stroke, part)];
        sim.run(&mut app, &mut cb, stroke, &mut strokes, &hit_tx);
        let live = app.live.last().expect("open until end + beat");
        match live.perf.hits()[0].judgment {
            perf::Judgment::Hit { target, error, .. } => {
                assert_eq!(target, 1);
                assert!((error - 2_400.0).abs() < 1.0, "{error}");
            }
            j => panic!("{j:?}"),
        }
        // end + beat = 120 000 frames heard.
        sim.run(&mut app, &mut cb, sim.wall(119_900.0), &mut vec![], &hit_tx);
        assert!(!app.live.is_empty());
        sim.run(&mut app, &mut cb, sim.wall(120_100.0), &mut vec![], &hit_tx);
        assert!(app.live.is_empty(), "closed past end + beat");
        assert!(!app.animating(), "nothing left to repaint for");
        let run = app.last_run.as_ref().unwrap();
        assert_eq!(run.ended_by, EndedBy::End);
        assert_eq!(run.hits.len(), 1);
        assert_eq!(
            run.misses.len(),
            2,
            "pulse 0 and the closing downbeat were not struck"
        );
        let written = app.written.clone().unwrap();
        assert!(written.starts_with(dir.join("runs")) && written.exists());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn the_last_miss_materialises_before_the_close() {
        let (mut app, mut cb, hit_tx, _tx) = app_with(Store::default());
        app.handle(perf_lesson("/l/p.md"));
        app.resume(0).unwrap();
        let mut sim = Sim::new(200);
        // The last window closes at 94 800 heard, the performance at 120 000.
        sim.run(&mut app, &mut cb, sim.wall(94_700.0), &mut vec![], &hit_tx);
        assert_eq!(app.live.last().unwrap().perf.misses().len(), 1);
        sim.run(&mut app, &mut cb, sim.wall(95_000.0), &mut vec![], &hit_tx);
        assert_eq!(app.live.last().unwrap().perf.misses().len(), 2);
        assert!(!app.live.is_empty());
    }

    #[test]
    fn a_stroke_after_the_trail_is_ignored() {
        let (mut app, mut cb, hit_tx, _tx) = app_with(Store::default());
        app.handle(perf_lesson("/l/p.md"));
        app.resume(0).unwrap();
        let part = part_f(&app);
        let mut sim = Sim::new(0);
        let stroke = sim.wall(120_050.0);
        let mut strokes = vec![(stroke, part)];
        sim.run(&mut app, &mut cb, stroke, &mut strokes, &hit_tx);
        let run = app.last_run.as_ref().unwrap();
        assert_eq!(run.ended_by, EndedBy::End);
        assert_eq!(run.hits.len(), 1);
        assert_eq!(run.hits[0].judgment, perf::RunJudgment::Ignored);
    }

    /// A late tap on the closing "one" of `P` (sparse part: pulses 0 and
    /// 15) is kept for a beat after the end: 150 ms late, outside the 100 ms
    /// window but within the 500 ms beat, so it is an extra annotated to the
    /// missed closing instance and the report counts it wide.
    #[test]
    fn a_late_closing_tap_reaches_the_run_as_a_wide_extra() {
        let dir = scratch("wide");
        let runs = dir.join("runs");
        let store = Store {
            runs: Some(runs.clone()),
            calibration: None,
        };
        let (mut app, mut cb, hit_tx, _tx) = app_with(store);
        app.handle(perf_lesson("/l/p.md"));
        app.resume(0).unwrap();
        let rx = subscribe(&mut app);
        let _ = lines(&rx);
        let part = part_f(&app);
        let mut sim = Sim::new(200);
        let mut strokes = vec![(sim.wall(96_000.0 + 7_200.0), part)];
        sim.run(
            &mut app,
            &mut cb,
            sim.wall(119_900.0),
            &mut strokes,
            &hit_tx,
        );
        assert!(!app.live.is_empty(), "open until end + beat");
        assert!(lines(&rx).iter().all(|l| !l.starts_with("ended")));
        assert_eq!(app.written, None);
        let files = |dir: &Path| std::fs::read_dir(dir).map_or(0, |d| d.count());
        assert_eq!(files(&runs), 0);
        sim.run(&mut app, &mut cb, sim.wall(120_100.0), &mut vec![], &hit_tx);
        assert!(app.live.is_empty(), "closed past end + beat");
        let written = app.written.clone().expect("run file written");
        assert_eq!(
            lines(&rx),
            [format!(
                "ended label=P by=end cycles=1/1 hit=0 miss=3 extra=1 run={} !",
                written.display()
            )]
        );
        assert!(written.starts_with(&runs) && written.exists());
        assert_eq!(files(&runs), 1);
        let run: Run = serde_json::from_str(&std::fs::read_to_string(&written).unwrap()).unwrap();
        assert_eq!(run.ended_by, EndedBy::End);
        assert_eq!(run.hits.len(), 1);
        let h = &run.hits[0];
        assert_eq!(
            (h.judgment, h.cycle, h.pulse),
            (perf::RunJudgment::Extra, 1, Some(0))
        );
        let text = crate::report::render(&run, None);
        // targets hit miss extra wide
        assert_eq!(
            &crate::report::tests::row_of(&text, "Overall", "all")[4..6],
            ["1", "1"],
            "{text}"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_stop_inside_the_trail_retires_the_run_by_stop() {
        let (mut app, mut cb, hit_tx, _tx) = app_with(Store::default());
        app.handle(perf_lesson("/l/p.md"));
        app.resume(0).unwrap();
        let rx = subscribe(&mut app);
        let _ = lines(&rx);
        let part = part_f(&app);
        let mut sim = Sim::new(200);
        let mut strokes = vec![(sim.wall(96_000.0 + 7_200.0), part)];
        sim.run(
            &mut app,
            &mut cb,
            sim.wall(104_000.0),
            &mut strokes,
            &hit_tx,
        );
        assert!(!app.live.is_empty(), "inside the trail");
        app.handle(Request::Stop);
        assert!(app.live.is_empty());
        assert_eq!(
            lines(&rx),
            ["ended label=P by=stop cycles=1/1 hit=0 miss=3 extra=1 !"]
        );
        let run = app.last_run.as_ref().unwrap();
        assert_eq!((run.ended_by, run.cycles_played), (EndedBy::Stop, 1));
        assert_eq!(run.misses.len(), 3, "pulse 0, pulse 15 and the closing one");
        assert_eq!(run.hits.len(), 1);
        assert_eq!(run.hits[0].judgment, perf::RunJudgment::Extra);
    }

    #[test]
    fn strokes_are_dropped_while_paused_after_stop_and_before_the_play_applies() {
        let (mut app, mut cb, hit_tx, _tx) = app_with(Store::default());
        app.handle(perf_lesson("/l/p.md"));
        app.resume(1).unwrap();
        let part = part_f(&app);
        let mut sim = Sim::new(0);
        // Into cycle 3 of the loop: both targets of cycles 1 and 2 and the
        // first of cycle 3 missed.
        sim.run(&mut app, &mut cb, sim.at(4.5), &mut vec![], &hit_tx);
        assert_eq!(app.live.last().unwrap().perf.misses().len(), 5);

        // Restarted but not yet applied: the snapshot still shows the old
        // run deep in cycle 3, and nothing is judged against it.
        app.start(1).unwrap();
        let now = sim.at(4.5);
        hit_tx
            .send((
                Hit {
                    at: now,
                    trigger: key_code("f"),
                    velocity: None,
                },
                part.clone(),
            ))
            .unwrap();
        app.judge_hits(now);
        let live = app.live.last().unwrap();
        assert!(live.perf.misses().is_empty() && live.perf.hits().is_empty());

        app.engine.pause().unwrap();
        cb.tick_at(sim.at(4.6), Duration::ZERO);
        let mut strokes = vec![(sim.at(4.7), part.clone())];
        send_due(&mut strokes, sim.at(4.7), &hit_tx);
        app.judge_hits(sim.at(4.7));
        assert!(app.live.last().unwrap().perf.hits().is_empty(), "paused");

        app.stop().unwrap();
        cb.tick_at(sim.at(4.8), Duration::ZERO);
        let mut strokes = vec![(sim.at(4.9), part)];
        send_due(&mut strokes, sim.at(4.9), &hit_tx);
        app.judge_hits(sim.at(4.9));
        assert!(app.live.is_empty());
        assert!(app.last_run.as_ref().unwrap().hits.is_empty(), "stopped");
    }

    /// 100 bpm at 48 kHz: frames a beat of the calibration block.
    const CAL_BEAT: f64 = 28_800.0;

    /// Plays the calibration block until click `until`, tapping each click
    /// in `clicks` 10 ms late; returns the simulated instant it ran to.
    fn calibrate(
        app: &mut App<()>,
        cb: &mut Callback,
        hit_tx: &mpsc::Sender<(Hit, Part)>,
        clicks: impl IntoIterator<Item = u32>,
        until: u32,
    ) -> Mono {
        let part = part_f(app);
        let mut sim = Sim::new(0);
        let mut strokes: Vec<(Mono, Part)> = clicks
            .into_iter()
            .map(|n| (sim.wall(n as f64 * CAL_BEAT + 480.0), part.clone()))
            .collect();
        let end = sim.wall(until as f64 * CAL_BEAT);
        sim.run(app, cb, end, &mut strokes, hit_tx);
        end
    }

    fn cal_store(name: &str) -> (PathBuf, Store) {
        let dir = scratch(name);
        let store = Store {
            runs: Some(dir.join("runs")),
            calibration: Some(dir.join("calibration.json")),
        };
        (dir, store)
    }

    #[test]
    fn calibration_stops_itself_at_enough_taps_and_saves_the_median_offset() {
        let (dir, store) = cal_store("cal");
        let path = store.calibration.clone().unwrap();
        let (mut app, mut cb, hit_tx, _tx) = app_with(store.clone());
        assert_eq!(app.calibration_status(), "uncalibrated");
        app.handle(Request::Calibrate);
        assert!(app.live.last().unwrap().perf.is_calibration());
        // Comes in on the fourth click, skips two, and taps on past the
        // 24th hit.
        let clicks = (3..32).filter(|n| *n != 10 && *n != 17);
        let end = calibrate(&mut app, &mut cb, &hit_tx, clicks, 40);
        assert!(app.live.is_empty());
        assert_eq!(app.transport_at(end), Transport::Stopped);
        let run = app.last_run.as_ref().unwrap();
        assert_eq!(run.ended_by, EndedBy::Enough);
        assert_eq!(run.hits.len(), perf::CALIBRATION_TAPS);
        let missed: Vec<u32> = run.misses.iter().map(|m| m.cycle * 4 + m.pulse).collect();
        assert_eq!(
            missed,
            [0, 1, 2, 10, 17],
            "the skips, and none after the stop"
        );
        let saved = Calibration::load(&path).unwrap().unwrap();
        assert!((saved.offset_ms - 10.0).abs() < 0.1, "{saved:?}");
        assert_eq!(saved.taps, perf::CALIBRATION_TAPS);
        assert_eq!(app.calibration_status(), "calibration: +10 ms");

        // A later window loads it, and judges a user lesson with it.
        let (mut again, _cb, _hit_tx, _tx) = app_with(store);
        assert_eq!(again.calibration_status(), "calibration: +10 ms");
        again.handle(perf_lesson("/l/p.md"));
        again.resume(0).unwrap();
        let run = again.live.last().unwrap().perf.run(EndedBy::Open);
        assert_eq!(run.offset_ms, saved.offset_ms);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn an_early_last_tap_stops_the_calibration_once_its_click_has_sounded() {
        let (mut app, mut cb, hit_tx, _tx) = app_with(Store::default());
        app.handle(Request::Calibrate);
        let part = part_f(&app);
        let mut sim = Sim::new(0);
        // Every tap 100 ms (4 800 frames) ahead of its click.
        let mut strokes: Vec<(Mono, Part)> = (1..=24)
            .map(|n| (sim.wall(n as f64 * CAL_BEAT - 4_800.0), part.clone()))
            .collect();
        let click = 24.0 * CAL_BEAT;
        let until = sim.wall(click - 2_400.0);
        sim.run(&mut app, &mut cb, until, &mut strokes, &hit_tx);
        let live = app.live.last().expect("the 24th click has not sounded");
        assert_eq!(live.perf.hit_count(), perf::CALIBRATION_TAPS);
        assert!(matches!(app.transport_at(until), Transport::Playing(_)));

        let until = sim.wall(click + CAL_BEAT);
        sim.run(&mut app, &mut cb, until, &mut strokes, &hit_tx);
        assert!(app.live.is_empty());
        assert_eq!(ended_by(&app), Some(EndedBy::Enough));
        assert_eq!(app.transport_at(until), Transport::Stopped);
    }

    #[test]
    fn a_calibration_whose_stop_cannot_be_sent_stops_on_a_later_pass() {
        let (mut app, mut cb, hit_tx, _tx) = app_with(Store::default());
        app.handle(Request::Calibrate);
        let part = part_f(&app);
        let mut sim = Sim::new(0);
        let mut strokes: Vec<(Mono, Part)> = (0..24)
            .map(|n| (sim.wall(n as f64 * CAL_BEAT + 480.0), part.clone()))
            .collect();
        let mut last = vec![strokes.pop().unwrap()];
        sim.run(
            &mut app,
            &mut cb,
            sim.wall(23.0 * CAL_BEAT),
            &mut strokes,
            &hit_tx,
        );
        // A full command queue refuses the stop.
        while app.engine.set_mute(0, false, 0).is_ok() {}
        let now = sim.wall(23.0 * CAL_BEAT + 960.0);
        send_due(&mut last, now, &hit_tx);
        app.judge_hits(now);
        let live = app.live.last().expect("the stop was not sent");
        assert_eq!(live.perf.hit_count(), perf::CALIBRATION_TAPS);
        assert!(live.perf.targets_heard());

        // The callback drains the queue; a later pass stops and closes.
        let until = sim.wall(24.0 * CAL_BEAT);
        sim.run(&mut app, &mut cb, until, &mut vec![], &hit_tx);
        assert_eq!(ended_by(&app), Some(EndedBy::Enough));
        assert_eq!(app.transport_at(until), Transport::Stopped);
    }

    #[test]
    fn a_user_part_plays_on_past_the_calibration_tap_count() {
        let (mut app, mut cb, hit_tx, _tx) = app_with(Store::default());
        app.handle(perf_lesson("/l/p.md"));
        app.resume(1).unwrap();
        let part = part_f(&app);
        let mut sim = Sim::new(0);
        // Q loops: targets at frames 0 and 90 000 of each 96 000-frame cycle.
        let mut strokes: Vec<(Mono, Part)> = (0..15u32)
            .flat_map(|k| [0.0, 90_000.0].map(|at| k as f64 * 96_000.0 + at + 480.0))
            .map(|h| (sim.wall(h), part.clone()))
            .collect();
        let until = sim.wall(15.0 * 96_000.0);
        sim.run(&mut app, &mut cb, until, &mut strokes, &hit_tx);
        let live = app.live.last().expect("still open");
        assert_eq!(live.perf.hit_count(), 30);
        assert!(live.perf.hit_count() > perf::CALIBRATION_TAPS);
        assert!(matches!(app.transport_at(until), Transport::Playing(_)));
        assert!(app.last_run.is_none());
    }

    #[test]
    fn a_stopped_calibration_saves_from_16_taps_and_says_why_below() {
        let (dir, store) = cal_store("cal-stop");
        let path = store.calibration.clone().unwrap();
        let (mut app, mut cb, hit_tx, _tx) = app_with(store);
        app.handle(Request::Calibrate);
        calibrate(&mut app, &mut cb, &hit_tx, 0..10, 12);
        app.stop().unwrap();
        assert_eq!(ended_by(&app), Some(EndedBy::Stop));
        assert!(!path.exists());
        assert_eq!(app.calibration, None);
        assert_eq!(
            app.calibration_status(),
            "calibration: not enough taps (10/16)"
        );

        app.handle(Request::Calibrate);
        calibrate(&mut app, &mut cb, &hit_tx, 0..16, 18);
        app.stop().unwrap();
        assert_eq!(ended_by(&app), Some(EndedBy::Stop));
        assert_eq!(Calibration::load(&path).unwrap().unwrap().taps, 16);
        assert_eq!(app.calibration_status(), "calibration: +10 ms");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn showing_a_lesson_over_a_calibration_saves_from_16_taps() {
        let (dir, store) = cal_store("cal-show");
        let path = store.calibration.clone().unwrap();
        let (mut app, mut cb, hit_tx, _tx) = app_with(store);
        app.handle(Request::Calibrate);
        calibrate(&mut app, &mut cb, &hit_tx, 0..16, 18);
        app.handle(perf_lesson("/l/p.md"));
        assert_eq!(ended_by(&app), Some(EndedBy::Show));
        assert_eq!(Calibration::load(&path).unwrap().unwrap().taps, 16);
        assert_eq!(app.calibration_status(), "calibration: +10 ms");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn leaving_an_untapped_calibration_keeps_the_stored_offset() {
        let (dir, store) = cal_store("cal-leave");
        let path = store.calibration.clone().unwrap();
        let stored = Calibration {
            offset_ms: -112.0,
            sd_ms: 20.0,
            taps: 24,
            measured: "2026-10-11T19:15:46Z".into(),
        };
        stored.save(&path).unwrap();
        let (mut app, mut cb, hit_tx, _tx) = app_with(store);
        assert_eq!(app.calibration_status(), "calibration: -112 ms");

        app.handle(Request::Calibrate);
        calibrate(&mut app, &mut cb, &hit_tx, [], 6);
        app.start(0).unwrap();
        assert_eq!(ended_by(&app), Some(EndedBy::Restart));
        assert_eq!(app.calibration.as_ref(), Some(&stored));
        assert_eq!(app.calibration_status(), "calibration: -112 ms");

        app.handle(perf_lesson("/l/p.md"));
        assert_eq!(ended_by(&app), Some(EndedBy::Show));
        assert_eq!(app.calibration.as_ref(), Some(&stored));
        assert_eq!(app.calibration_status(), "calibration: -112 ms");
        assert_eq!(Calibration::load(&path).unwrap().unwrap(), stored);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn the_calibration_header_shows_progress_and_other_blocks_the_tally() {
        let (mut app, mut cb, hit_tx, _tx) = app_with(Store::default());
        app.handle(Request::Calibrate);
        calibrate(&mut app, &mut cb, &hit_tx, [1, 2, 4], 6);
        let perf = &app.live.last().unwrap().perf;
        assert_eq!(performance_text(perf), "taps 3/24 · median +10 ms");
        app.handle(Request::Stop);
        let closed = &app.closed.last().unwrap().1;
        assert_eq!(performance_text(closed), "taps 3/24 · median +10 ms");
        app.handle(player_lesson());
        app.handle(Request::Play {
            label: Some("A".into()),
        });
        app.resume(0).unwrap();
        let perf = &app.live.last().unwrap().perf;
        assert_eq!(performance_text(perf), "hit 0 · miss 0 · extra 0");
    }

    #[test]
    fn the_status_line_shows_the_audio_period_and_latency() {
        let (app, mut cb, _tx) = app();
        assert_eq!(app.audio_status(), "audio: –");
        cb.tick_frames(Mono(1_000), Duration::from_micros(8_900), 128);
        assert_eq!(app.audio_status(), "audio: 128 frames · 9 ms");
    }

    #[test]
    fn a_performance_records_the_reported_latency() {
        let (mut app, mut cb, _tx) = app();
        app.handle(Request::Calibrate);
        let run = app.live.last().unwrap().perf.run(EndedBy::Open);
        assert_eq!(run.latency_ms, None, "no callback yet");
        cb.tick_frames(Mono(1_000), Duration::from_millis(9), 128);
        app.start(0).unwrap();
        let run = app.live.last().unwrap().perf.run(EndedBy::Open);
        assert_eq!(run.latency_ms, Some(9.0));
    }

    #[test]
    fn calibration_status_text() {
        let c = Calibration {
            offset_ms: -7.4,
            sd_ms: 3.0,
            taps: 30,
            measured: "x".into(),
        };
        assert_eq!(calibration_text(Some(&c), None), "calibration: -7 ms");
        assert_eq!(calibration_text(None, None), "uncalibrated");
        assert_eq!(calibration_text(Some(&c), Some("n")), "n");
    }

    #[test]
    fn an_unwritable_run_directory_reaches_the_status_line() {
        let dir = scratch("unwritable");
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("not-a-dir");
        std::fs::write(&file, "").unwrap();
        let store = Store {
            runs: Some(file.join("runs")),
            calibration: None,
        };
        let (mut app, mut cb, hit_tx, _tx) = app_with(store);
        app.handle(perf_lesson("/l/p.md"));
        app.resume(0).unwrap();
        let mut sim = Sim::new(0);
        sim.run(&mut app, &mut cb, sim.at(2.6), &mut vec![], &hit_tx);
        assert_eq!(ended_by(&app), Some(EndedBy::End));
        assert!(app.notice.as_ref().unwrap().starts_with("run not saved: "));
        assert!(app.written.is_none());
        let _ = std::fs::remove_dir_all(dir);
    }

    /// A `wait` on `app`: its event lines so far, ` !` marking a terminal one.
    fn subscribe(app: &mut App<()>) -> mpsc::Receiver<Event> {
        let (tx, rx) = mpsc::channel();
        assert!(app.wait(tx).ok);
        rx
    }

    fn lines(rx: &mpsc::Receiver<Event>) -> Vec<String> {
        rx.try_iter()
            .map(|e| format!("{}{}", e.line(), if e.terminal { " !" } else { "" }))
            .collect()
    }

    /// The boundary orders asserted here are the `wait` contract stated in
    /// `starter/instructor.md`: a block without a part has its `ended …
    /// next=B` before `started B`; a block with a part that plays through
    /// has its `ended by=end` after `started B`, once its judged span
    /// closes. Flipping either changes the contract, not just this test.
    #[test]
    fn events_are_emitted_once_per_block_play() {
        // A listen-only score played through, restarted and stopped.
        let (mut app, mut cb, hit_tx, _tx) = app_with(Store::default());
        let rx = subscribe(&mut app);
        assert_eq!(lines(&rx), ["idle"]);
        app.handle(score_lesson(false));
        app.handle(Request::Play {
            label: Some("guided".into()),
        });
        let mut sim = Sim::new(20);
        sim.run(&mut app, &mut cb, sim.wall(500_000.0), &mut vec![], &hit_tx);
        assert_eq!(
            lines(&rx),
            [
                "shown lesson=/abs/lessons/score.md",
                "started label=guided",
                "ended label=guided by=end next=alone cycles=2/2",
                "started label=alone",
                "ended label=alone by=end cycles=2/2 !",
            ]
        );
        app.handle(Request::Play {
            label: Some("alone".into()),
        });
        app.handle(Request::Play {
            label: Some("alone".into()),
        });
        app.handle(Request::Stop);
        app.handle(Request::Stop);
        assert_eq!(
            lines(&rx),
            [
                "started label=alone",
                "ended label=alone by=restart cycles=0/2",
                "started label=alone",
                "ended label=alone by=stop cycles=0/2 !",
            ],
            "a second stop ends nothing"
        );

        // A score with a part on each block: armed, then started with space.
        let (mut app, mut cb, hit_tx, _tx) = app_with(Store::default());
        app.handle(part_score(&[0, 6, 15], "f"));
        let rx = subscribe(&mut app);
        app.handle(Request::Play {
            label: Some("guided".into()),
        });
        app.resume(0).unwrap();
        let mut sim = Sim::new(20);
        sim.run(
            &mut app,
            &mut cb,
            sim.wall(ORIGIN_B + 2.0 * 96_000.0 + 36_000.0),
            &mut vec![],
            &hit_tx,
        );
        assert_eq!(
            lines(&rx),
            [
                "idle",
                "armed label=guided",
                "started label=guided",
                "started label=alone",
                "ended label=guided by=end next=alone cycles=2/2 hit=0 miss=6 extra=0",
                "ended label=alone by=end cycles=2/2 hit=0 miss=7 extra=0 !",
            ]
        );

        // Restart, switch, show and quit of blocks with and without a part.
        let (mut app, _cb, _tx) = self::app();
        app.handle(perf_lesson("/l/p.md"));
        let rx = subscribe(&mut app);
        app.start(1).unwrap();
        app.start(1).unwrap();
        app.resume(2).unwrap();
        app.handle(perf_lesson("/l/q.md"));
        app.start(1).unwrap();
        app.quit();
        assert_eq!(
            lines(&rx),
            [
                "idle",
                "started label=Q",
                "ended label=Q by=restart cycles=0 hit=0 miss=0 extra=0",
                "started label=Q",
                "ended label=Q by=switch cycles=0 hit=0 miss=0 extra=0",
                "started label=N",
                "ended label=N by=show cycles=0",
                "shown lesson=/l/q.md",
                "started label=Q",
                "ended label=Q by=quit cycles=0 hit=0 miss=0 extra=0",
            ]
        );
    }

    #[test]
    fn arming_over_a_long_playing_block_judges_nothing_before_space() {
        // A looping listen-only block, then `part_score`'s guided → alone.
        let Request::Show {
            path,
            mut lesson,
            play,
        } = part_score(&[0, 6, 15], "f")
        else {
            unreachable!()
        };
        lesson.segments.insert(
            0,
            Segment::Rhythm(exercise(json!({
                "label": "L", "tempo": 120, "beats": 4,
                "voices": [{ "name": "bell", "instrument": "bell", "pulses": 4,
                             "onsets": [0] }]
            }))),
        );
        let dir = scratch("arm-over");
        let store = Store {
            runs: Some(dir.clone()),
            calibration: None,
        };
        let (mut app, mut cb, hit_tx, _tx) = app_with(store);
        app.handle(Request::Show { path, lesson, play });
        app.start(0).unwrap();
        let mut sim = Sim::new(20);
        sim.run(
            &mut app,
            &mut cb,
            sim.wall(1_000_000.0),
            &mut vec![],
            &hit_tx,
        );
        let rx = subscribe(&mut app);
        lines(&rx); // the state event, read on the wall clock
        app.handle(Request::Play {
            label: Some("guided".into()),
        });
        // Passes before and after the callback applies the new score.
        let t = sim.wall(1_000_000.0);
        app.follow_score(t);
        app.judge_hits(t);
        sim.run(
            &mut app,
            &mut cb,
            sim.wall(1_100_000.0),
            &mut vec![],
            &hit_tx,
        );
        assert_eq!(
            lines(&rx),
            ["ended label=L by=stop cycles=10 !", "armed label=guided",]
        );
        assert!(app.live.is_empty());
        assert!(app.last_run.is_none());
        assert_eq!(app.armed, Some(1));
        assert!(!dir.exists(), "no run file written");
    }

    #[test]
    fn restart_and_switch_report_the_cycles_heard() {
        let (mut app, mut cb, hit_tx, _tx) = app_with(Store::default());
        app.handle(perf_lesson("/l/p.md"));
        // N: 90 bpm, 4 beats, no count-in: 128 000 frames a cycle.
        app.start(2).unwrap();
        let mut sim = Sim::new(0);
        sim.run(&mut app, &mut cb, sim.at(10.0), &mut vec![], &hit_tx);
        let rx = subscribe(&mut app);
        lines(&rx); // the state event, read on the wall clock
        app.start(2).unwrap();
        sim.run(&mut app, &mut cb, sim.at(13.0), &mut vec![], &hit_tx);
        app.start(0).unwrap();
        assert_eq!(
            lines(&rx),
            [
                "ended label=N by=restart cycles=3",
                "started label=N",
                "ended label=N by=switch cycles=1",
                "started label=P",
            ]
        );
    }

    #[test]
    fn a_report_names_the_open_run_of_the_block_being_heard() {
        let (mut app, mut cb, hit_tx, _tx) = app_with(Store::default());
        app.handle(part_score(&[0, 6, 15], "f"));
        app.start(0).unwrap();
        let mut sim = Sim::new(20);
        let label = |r: Response| r.run.map(|run| run.label);
        let before = sim.wall(ORIGIN_B - 1_000.0);
        sim.run(&mut app, &mut cb, before, &mut vec![], &hit_tx);
        assert_eq!(app.live.len(), 2, "both performances open");
        assert_eq!(app.live.last().map(|l| l.block), Some(1));
        assert_eq!(label(app.report_at(before)).as_deref(), Some("guided"));
        let after = sim.wall(ORIGIN_B + 1_000.0);
        sim.run(&mut app, &mut cb, after, &mut vec![], &hit_tx);
        assert_eq!(app.live.len(), 2, "guided's judged span is open");
        assert_eq!(label(app.report_at(after)).as_deref(), Some("alone"));
    }

    #[test]
    fn a_report_after_a_stop_across_a_boundary_names_no_other_blocks_file() {
        let dir = scratch("stop-boundary");
        let store = Store {
            runs: Some(dir.clone()),
            calibration: None,
        };
        let (mut app, mut cb, hit_tx, _tx) = app_with(store);
        app.handle(part_score(&[0, 6, 15], "f"));
        app.start(0).unwrap();
        let mut sim = Sim::new(20);
        let until = sim.wall(ORIGIN_B - 1_000.0);
        sim.run(&mut app, &mut cb, until, &mut vec![], &hit_tx);
        assert_eq!(app.live.len(), 2, "both performances open");
        app.handle(Request::Stop);
        let report = app.report_at(until);
        let run = report.run.unwrap();
        assert_eq!((run.label.as_str(), run.hits.len()), ("alone", 0));
        assert!(run.misses.is_empty());
        assert_eq!(report.run_path, None, "alone wrote no file");
        let files: Vec<_> = std::fs::read_dir(&dir).unwrap().collect();
        assert_eq!(files.len(), 1, "guided's run");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_calibration_that_stops_itself_ends_a_wait() {
        let (mut app, mut cb, hit_tx, _tx) = app_with(Store::default());
        app.handle(Request::Calibrate);
        let rx = subscribe(&mut app);
        calibrate(&mut app, &mut cb, &hit_tx, 0..24, 26);
        let lines = lines(&rx);
        let last = lines.last().unwrap();
        assert!(
            last.starts_with("ended label=cal by=enough ") && last.ends_with(" !"),
            "{lines:?}"
        );
    }

    #[test]
    fn wait_state_event_reports_a_finished_performance_at_once() {
        let (mut app, mut cb, hit_tx, _tx) = app_with(Store::default());
        app.handle(perf_lesson("/l/p.md"));
        app.resume(0).unwrap();
        let mut sim = Sim::new(0);
        sim.run(&mut app, &mut cb, sim.at(2.6), &mut vec![], &hit_tx);
        assert_eq!(ended_by(&app), Some(EndedBy::End));
        let rx = subscribe(&mut app);
        assert_eq!(
            lines(&rx),
            ["ended label=P by=end cycles=1/1 hit=0 miss=3 extra=0 !"]
        );
        assert!(app.subscribers.is_empty(), "that wait is over");

        app.handle(Request::Play {
            label: Some("P".into()),
        });
        assert_eq!(lines(&subscribe(&mut app)), ["armed label=P"]);
        app.resume(0).unwrap();
        cb.tick();
        assert_eq!(lines(&subscribe(&mut app)), ["playing label=P cycle=1/1"]);
        app.engine.pause().unwrap();
        cb.tick();
        assert_eq!(lines(&subscribe(&mut app)), ["paused label=P"]);
        assert_eq!(
            app.subscribers.len(),
            2,
            "the first, its client gone, was dropped when `started` found no one"
        );
    }
}
