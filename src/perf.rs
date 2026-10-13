//! Judging a performance: the user's strokes against the targets of the
//! player voices, and the raw run record a performance produces.
//!
//! Positions are mixer frames counted from the start of the score being
//! played (the frame `engine::heard` returns), minus the calibration offset;
//! a block's timeline begins at its `origin` among them. A target
//! *instance* is one target in one cycle.

use std::collections::{BTreeMap, HashSet};
use std::env;
use std::ffi::OsString;
use std::fs;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::model::{Ending, Exercise};
use crate::timeline::Timeline;

/// A stroke within this many ms of its target is *on*; otherwise early or
/// late.
pub const ON_MS: f64 = 20.0;
/// The `djembot` field of a run file.
pub const RUN_FORMAT: u32 = 3;
/// Matched taps a calibration run needs before its offset is kept.
pub const MIN_CALIBRATION_TAPS: usize = 16;
/// Matched taps at which the window stops the calibration block.
pub const CALIBRATION_TAPS: usize = 24;
/// The lesson name of the built-in calibration lesson.
pub const CALIBRATION_NAME: &str = "<calibration>";
/// The built-in calibration lesson: one looping block, `cal`, a click on
/// every beat tapped on `f` without sound, with no count-in and a window
/// of half a beat.
pub const CALIBRATION_LESSON: &str = r#"# Calibration

Listen to the clicks. When you have the beat, tap `f` on every click, as
evenly as you can; it does not matter when you come in or if you skip some.
The block stops by itself once it has enough taps.

```djembot
{ "label": "cal", "title": "Calibration", "tempo": 100, "beats": 4,
  "window_ms": 300,
  "voices": [ { "name": "click", "instrument": "click", "pulses": 4,
                "onsets": [0, 1, 2, 3], "keys": "f", "mute": false,
                "silent": true } ] }
```
"#;

/// One onset of a player voice, positioned within the cycle.
#[derive(Clone, Debug, PartialEq)]
pub struct Target {
    /// Voice index (0-based).
    pub voice: usize,
    pub pulse: u32,
    pub tone: char,
    /// Frames into the cycle.
    pub at: u64,
}

/// What ended a performance; `Open` marks a snapshot of one still running.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EndedBy {
    Stop,
    Restart,
    Show,
    Switch,
    Quit,
    End,
    /// The window stopped a calibration block that had enough taps.
    Enough,
    Open,
}

impl EndedBy {
    /// The name a run file and the report spell it with.
    pub fn as_str(self) -> &'static str {
        match self {
            EndedBy::Stop => "stop",
            EndedBy::Restart => "restart",
            EndedBy::Show => "show",
            EndedBy::Switch => "switch",
            EndedBy::Quit => "quit",
            EndedBy::End => "end",
            EndedBy::Enough => "enough",
            EndedBy::Open => "open",
        }
    }
}

/// How a stroke was judged.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Judgment {
    /// Matched target `target` in cycle `cycle`; `error` in frames,
    /// negative when early.
    Hit {
        cycle: u32,
        target: usize,
        error: f64,
    },
    /// Not matched: no unclaimed target within the window.
    Extra,
    /// Outside the judged span, or in the count-in without matching a
    /// target.
    Ignored,
}

/// One stroke on a player voice.
#[derive(Clone, Debug, PartialEq)]
pub struct HitRecord {
    /// Frame after calibration.
    pub frame: f64,
    pub voice: usize,
    pub tone: char,
    pub velocity: Option<f32>,
    pub judgment: Judgment,
}

/// A target instance whose window closed without a matching stroke.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MissRecord {
    pub cycle: u32,
    pub target: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Timing {
    Early,
    On,
    Late,
}

/// Early, on (within `ON_MS`) or late, from a signed error in ms.
pub fn timing(error_ms: f64) -> Timing {
    if error_ms < -ON_MS {
        Timing::Early
    } else if error_ms > ON_MS {
        Timing::Late
    } else {
        Timing::On
    }
}

/// Counts for the live line in the block header.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Tally {
    pub hit: usize,
    pub miss: usize,
    pub extra: usize,
    /// Mean signed error of the hits in ms; `None` without hits.
    pub mean_ms: Option<f64>,
}

/// One judgment as the grid and the necklace draw it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Mark {
    /// Voice index (0-based).
    pub voice: usize,
    /// The cycle it belongs to: the matched or missed instance's, else the
    /// one the stroke fell in.
    pub cycle: i64,
    /// Where it is drawn, as a fraction of the cycle: the stroke's position
    /// for a hit (below 0 for an early stroke on pulse 0) or an extra, the
    /// target's for a miss.
    pub frac: f64,
    pub kind: MarkKind,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum MarkKind {
    Hit(Timing),
    Miss,
    Extra,
}

/// What a run records besides the strokes: the block and the conditions it
/// was played under.
#[derive(Clone, Debug, PartialEq)]
struct RunMeta {
    lesson: String,
    label: String,
    title: Option<String>,
    tempo: f64,
    beats: u32,
    count_in: u32,
    window_ms: f64,
    offset_ms: f64,
    latency_ms: Option<f64>,
    input: String,
    started: String,
    parts: Vec<RunPart>,
}

/// What a performance records about how it was played.
#[derive(Clone, Copy, Debug)]
pub struct Conditions<'a> {
    /// The lesson file.
    pub lesson: &'a str,
    /// The keyboard status.
    pub input: &'a str,
    /// The calibration offset, subtracted from every stroke (0 for the
    /// calibration block itself).
    pub offset_ms: f64,
    /// The device-reported output latency, recorded only.
    pub latency_ms: Option<f64>,
}

/// The beat lengths, in frames, of the links a block of a score is chained
/// to: `before`, the link it was entered from attacca; `after`, the link
/// that follows it attacca. Both `None` for a block played on its own.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Neighbours {
    pub before: Option<u64>,
    pub after: Option<u64>,
}

/// A window of `window_ms` in frames at `rate`.
pub fn window_frames(window_ms: f64, rate: u32) -> u64 {
    (window_ms * rate as f64 / 1000.0).round() as u64
}

/// How far a judged span reaches past its block's edge on a side: one
/// window, or across an attacca boundary the neighbour's beat when that is
/// longer.
fn reach(window: u64, neighbour_beat: Option<u64>) -> u64 {
    neighbour_beat.map_or(window, |beat| beat.max(window))
}

/// The judged frame the span of a block whose cycle 0 begins at `start`
/// begins at, given its window in frames and the beat of the link it was
/// entered from.
pub fn span_start(start: u64, window: u64, before: Option<u64>) -> f64 {
    start as f64 - reach(window, before) as f64
}

/// Test conditions: `lesson`, one keyboard device, `offset_ms` and
/// `latency_ms`.
#[cfg(test)]
pub fn conditions(lesson: &str, offset_ms: f64, latency_ms: Option<f64>) -> Conditions<'_> {
    Conditions {
        lesson,
        input: "1 device",
        offset_ms,
        latency_ms,
    }
}

/// One played block's judging state.
#[derive(Clone, Debug)]
pub struct Performance {
    meta: RunMeta,
    /// Player voices only, sorted by `at`.
    targets: Vec<Target>,
    cycle_len: u64,
    /// The frame cycle 0 begins at: the block's origin plus its count-in.
    start: u64,
    cycles: Option<u32>,
    /// The frame a finite block ends at.
    end: Option<u64>,
    /// Whether the block ends on a closing downbeat: the targets on pulse 0
    /// have one more instance, in cycle `cycles`.
    closing: bool,
    rate: u32,
    /// Half-width of the matching window, frames.
    window: u64,
    /// How far before `start` the judged span begins, frames.
    lead: u64,
    /// How far after `end` the judged span ends, frames.
    trail: u64,
    /// Whether the block was entered attacca: it has no count-in, and the
    /// time before `start` is the boundary region with the block before.
    attacca: bool,
    /// Whether no block follows attacca: instances after the last cycle
    /// can annotate an extra.
    last: bool,
    /// Calibration offset, frames, subtracted from every stroke.
    offset: f64,
    /// (cycle, target index) instances already matched.
    claimed: HashSet<(u32, usize)>,
    /// Instances below this frame have been judged hit or miss.
    judged_to: f64,
    /// The latest heard frame `advance` has seen.
    heard_to: f64,
    hits: Vec<HitRecord>,
    misses: Vec<MissRecord>,
}

impl Performance {
    /// A performance of `ex`, compiled as `tl` at `rate`, whose timeline
    /// begins at mixer frame `origin`, chained to `neighbours` in its
    /// score, played under `conditions`.
    pub fn new(
        ex: &Exercise,
        tl: &Timeline,
        origin: u64,
        neighbours: Neighbours,
        rate: u32,
        conditions: Conditions,
    ) -> Performance {
        let Conditions {
            lesson,
            input,
            offset_ms,
            latency_ms,
        } = conditions;
        let mut targets: Vec<Target> = tl
            .events
            .iter()
            .filter(|e| ex.voices.get(e.voice).is_some_and(|v| !v.keys.is_empty()))
            .map(|e| Target {
                voice: e.voice,
                pulse: e.pulse,
                tone: ex.voices[e.voice]
                    .onsets
                    .iter()
                    .find(|o| o.at == e.pulse)
                    .map_or_else(|| ex.voices[e.voice].instrument.default_tone(), |o| o.tone),
                at: e.at,
            })
            .collect();
        targets.sort_by_key(|t| (t.at, t.voice));
        let parts = ex
            .player_voices()
            .map(|i| {
                let v = &ex.voices[i];
                RunPart {
                    voice: i + 1,
                    name: v.name.clone(),
                    instrument: v.instrument.name(),
                    keys: v
                        .keys
                        .iter()
                        .map(|k| (k.tone.to_string(), k.key.clone()))
                        .collect(),
                    pulses: v.pulses,
                    onsets: v.onsets.iter().map(|o| o.at).collect(),
                }
            })
            .collect();
        let window_ms = ex.window_ms();
        let window = window_frames(window_ms, rate);
        Performance {
            meta: RunMeta {
                lesson: lesson.to_string(),
                label: ex.label.clone(),
                title: ex.title.clone(),
                tempo: ex.tempo,
                beats: ex.beats,
                count_in: ex.count_in,
                window_ms,
                offset_ms,
                latency_ms,
                input: input.to_string(),
                started: utc(SystemTime::now()),
                parts,
            },
            targets,
            cycle_len: tl.cycle_len,
            start: origin + tl.count_in_len,
            cycles: tl.cycles,
            end: tl.end.map(|e| origin + e),
            closing: !tl.closing.is_empty(),
            rate,
            window,
            lead: reach(window, neighbours.before),
            // TODO(closing-tap-trail): one window after the end drops a late
            // closing tap that would be a wide extra anywhere else.
            trail: reach(window, neighbours.after),
            attacca: neighbours.before.is_some(),
            last: neighbours.after.is_none(),
            offset: offset_ms * rate as f64 / 1000.0,
            claimed: HashSet::new(),
            judged_to: f64::NEG_INFINITY,
            heard_to: f64::NEG_INFINITY,
            hits: Vec::new(),
            misses: Vec::new(),
        }
    }

    #[cfg(test)]
    pub fn targets(&self) -> &[Target] {
        &self.targets
    }

    #[cfg(test)]
    pub fn hits(&self) -> &[HitRecord] {
        &self.hits
    }

    #[cfg(test)]
    pub fn misses(&self) -> &[MissRecord] {
        &self.misses
    }

    /// Frames to ms.
    pub fn ms(&self, frames: f64) -> f64 {
        frames * 1000.0 / self.rate as f64
    }

    /// The frame of target `t` in cycle `cycle`.
    pub fn instance(&self, cycle: u32, t: usize) -> f64 {
        self.instance_at(cycle as i64, t)
    }

    /// The frame target `t` would have in cycle `cycle`, played or not.
    fn instance_at(&self, cycle: i64, t: usize) -> f64 {
        self.start as f64 + cycle as f64 * self.cycle_len as f64 + self.targets[t].at as f64
    }

    /// Where `frame` falls, in cycles from the start of cycle 0; negative
    /// before it.
    fn cycle_pos(&self, frame: f64) -> f64 {
        (frame - self.start as f64) / self.cycle_len as f64
    }

    /// The cycle `frame` falls in; negative in the count-in.
    pub fn cycle_of(&self, frame: f64) -> i64 {
        self.cycle_pos(frame).floor() as i64
    }

    /// Whether target `t` has an instance in cycle `k`: every target in
    /// every played cycle, and the targets on pulse 0 in the closing cycle
    /// `cycles` of a block that ends on the downbeat.
    fn instance_exists(&self, k: i64, t: usize) -> bool {
        k >= 0
            && match self.cycles {
                None => true,
                Some(c) => {
                    k < c as i64 || (self.closing && k == c as i64 && self.targets[t].pulse == 0)
                }
            }
    }

    /// The frame a stroke heard at `heard` (a mixer frame, before
    /// calibration) is judged at.
    pub fn judged_frame(&self, heard: f64) -> f64 {
        heard - self.offset
    }

    /// Whether target `t` in cycle `k` can annotate an extra: an instance
    /// that exists, or one beyond the ends of the score — cycle −1 of a
    /// block not entered attacca, cycle `cycles` and later of the last
    /// block. Between blocks of a score the neighbour's real targets stand
    /// there instead.
    fn annotates(&self, k: i64, t: usize) -> bool {
        self.instance_exists(k, t)
            || (k < 0 && !self.attacca)
            || (self.last && self.cycles.is_some_and(|c| k >= c as i64))
    }

    /// Whether the judged `frame` is inside the judged span: from `lead`
    /// before cycle 0 to `trail` after the end of a finite block.
    fn in_span(&self, frame: f64) -> bool {
        frame >= self.start as f64 - self.lead as f64
            && self.end.is_none_or(|e| frame <= (e + self.trail) as f64)
    }

    /// Every instance of `voice`'s targets in `frame`'s cycle and either
    /// neighbour, played or not, in cycle then target order, as `(cycle,
    /// target, frame − instance)`.
    fn instances_near(&self, frame: f64, voice: usize) -> impl Iterator<Item = (i64, usize, f64)> {
        let c = self.cycle_of(frame);
        (c - 1..=c + 1).flat_map(move |k| {
            self.targets
                .iter()
                .enumerate()
                .filter(move |(_, target)| target.voice == voice)
                .map(move |(t, _)| (k, t, frame - self.instance_at(k, t)))
        })
    }

    /// The instance of `voice`'s targets nearest the judged `frame` within
    /// the window, in the frame's cycle or either neighbour, as `(distance,
    /// cycle, target)`; `None` when there is none or `frame` is outside the
    /// judged span.
    fn nearest_within(&self, frame: f64, voice: usize) -> Option<(f64, u32, usize)> {
        if !self.in_span(frame) {
            return None;
        }
        let w = self.window as f64;
        self.instances_near(frame, voice)
            .filter(|&(k, t, e)| self.instance_exists(k, t) && e.abs() <= w)
            .min_by(|a, b| a.2.abs().total_cmp(&b.2.abs()))
            .map(|(k, t, e)| (e.abs(), k as u32, t))
    }

    /// The distance in frames from a stroke on `voice` heard at `heard` to
    /// the instance `judge` would match, without matching it: `None` when
    /// `judge` would not judge the stroke a hit.
    pub fn candidate(&self, heard: f64, voice: usize) -> Option<f64> {
        self.nearest_within(self.judged_frame(heard), voice)
            .filter(|&(_, k, t)| !self.claimed.contains(&(k, t)))
            .map(|(d, _, _)| d)
    }

    /// The distance in frames from a stroke on `voice` heard at `heard` to
    /// the nearest instance of the voice's targets that exists, claimed or
    /// not, at any distance, in the judged frame's cycle or either
    /// neighbour; `None` when the frame is outside the judged span or there
    /// is no such instance.
    pub fn nearest(&self, heard: f64, voice: usize) -> Option<f64> {
        let frame = self.judged_frame(heard);
        if !self.in_span(frame) {
            return None;
        }
        self.instances_near(frame, voice)
            .filter(|&(k, t, _)| self.instance_exists(k, t))
            .map(|(_, _, e)| e.abs())
            .min_by(f64::total_cmp)
    }

    /// Judges a stroke on `voice` heard at `heard` (a mixer frame, before
    /// calibration). The nearest instance of the voice's targets within the
    /// window, in the stroke's cycle or either neighbour, is matched unless
    /// already claimed; a claimed or absent one makes the stroke extra.
    /// Strokes outside the judged span are ignored, and so are strokes in
    /// the count-in that match nothing; a block entered attacca has no
    /// count-in, so such a stroke before its cycle 0 is extra.
    pub fn judge(
        &mut self,
        heard: f64,
        voice: usize,
        tone: char,
        velocity: Option<f32>,
    ) -> Judgment {
        let frame = self.judged_frame(heard);
        let judgment = if !self.in_span(frame) {
            Judgment::Ignored
        } else {
            match self.nearest_within(frame, voice) {
                Some((_, k, t)) if !self.claimed.contains(&(k, t)) => {
                    self.claimed.insert((k, t));
                    // A stroke whose window `advance` closed before the
                    // stroke was drained is still a hit, not a miss.
                    self.misses.retain(|m| (m.cycle, m.target) != (k, t));
                    Judgment::Hit {
                        cycle: k,
                        target: t,
                        error: frame - self.instance(k, t),
                    }
                }
                _ if frame < self.start as f64 && !self.attacca => Judgment::Ignored,
                _ => Judgment::Extra,
            }
        };
        self.hits.push(HitRecord {
            frame,
            voice,
            tone,
            velocity,
            judgment,
        });
        judgment
    }

    /// Records as missed every unclaimed instance whose window has closed by
    /// `heard` (a mixer frame, before calibration): `instance + window <
    /// heard − offset`.
    pub fn advance(&mut self, heard: f64) {
        let heard = heard - self.offset;
        if heard.is_finite() {
            self.heard_to = self.heard_to.max(heard);
        } else if self.end.is_none() {
            return;
        }
        let upto = heard - self.window as f64;
        if upto <= self.judged_to {
            return;
        }
        let first = self.cycle_of(self.judged_to).max(0);
        let mut last = self.cycle_of(upto);
        if let Some(c) = self.cycles {
            last = last.min(c as i64);
        }
        for k in first..=last {
            for t in 0..self.targets.len() {
                if !self.instance_exists(k, t) {
                    continue;
                }
                let k = k as u32;
                let at = self.instance(k, t);
                if at >= self.judged_to && at < upto && !self.claimed.contains(&(k, t)) {
                    self.misses.push(MissRecord {
                        cycle: k,
                        target: t,
                    });
                }
            }
        }
        self.judged_to = upto;
    }

    /// The mixer frame a finite block ends at; `None` when it loops.
    pub fn end(&self) -> Option<u64> {
        self.end
    }

    /// The mixer frame cycle 0 begins at.
    #[cfg(test)]
    pub fn start(&self) -> u64 {
        self.start
    }

    /// Whether the judged span of a finite block has ended by `heard` (a
    /// mixer frame, before calibration): later strokes are ignored.
    pub fn closed(&self, heard: f64) -> bool {
        self.end
            .is_some_and(|e| heard - self.offset > (e + self.trail) as f64)
    }

    /// Every unclaimed instance of a finite block becomes a miss. Does
    /// nothing on a looping block.
    pub fn finish(&mut self) {
        if let Some(end) = self.end {
            self.heard_to = self.heard_to.max(end as f64);
            self.advance(f64::INFINITY);
        }
    }

    /// Cycles begun on the heard clock, as far as `advance` has seen.
    pub fn cycles_played(&self) -> u32 {
        if self.heard_to < self.start as f64 {
            return 0;
        }
        let begun = (self.cycle_of(self.heard_to) + 1) as u32;
        self.cycles.map_or(begun, |c| begun.min(c))
    }

    pub fn tally(&self) -> Tally {
        let mut tally = Tally {
            hit: 0,
            miss: self.misses.len(),
            extra: 0,
            mean_ms: None,
        };
        let mut sum = 0.0;
        for h in &self.hits {
            match h.judgment {
                Judgment::Hit { error, .. } => {
                    tally.hit += 1;
                    sum += error;
                }
                Judgment::Extra => tally.extra += 1,
                Judgment::Ignored => {}
            }
        }
        if tally.hit > 0 {
            tally.mean_ms = Some(self.ms(sum / tally.hit as f64));
        }
        tally
    }

    /// Whether this is a performance of the built-in calibration lesson.
    pub fn is_calibration(&self) -> bool {
        self.meta.lesson == CALIBRATION_NAME
    }

    /// The errors of the matched strokes so far, in ms.
    fn hit_errors_ms(&self) -> impl Iterator<Item = f64> + '_ {
        self.hits.iter().filter_map(|h| match h.judgment {
            Judgment::Hit { error, .. } => Some(self.ms(error)),
            _ => None,
        })
    }

    /// Matched strokes so far.
    pub fn hit_count(&self) -> usize {
        self.hit_errors_ms().count()
    }

    /// Whether the target of every matched stroke has reached the speakers,
    /// as far as `advance` has seen. A stroke ahead of its target is judged
    /// before the target sounds.
    pub fn targets_heard(&self) -> bool {
        self.hits.iter().all(|h| match h.judgment {
            Judgment::Hit { error, .. } => h.frame - error <= self.heard_to,
            _ => true,
        })
    }

    /// The median error of the matched strokes in ms; `None` without one.
    pub fn median_ms(&self) -> Option<f64> {
        median(&self.hit_errors_ms().collect::<Vec<_>>())
    }

    /// The cycle the heard clock is in, as far as `advance` has seen: 0
    /// until cycle 0 begins, the last cycle once a finite block has ended.
    pub fn current_cycle(&self) -> u32 {
        self.cycles_played().saturating_sub(1)
    }

    /// The hits, extras and misses of cycle `from` and later, positioned in
    /// their cycle. Ignored strokes are not marked.
    pub fn marks_since(&self, from: i64) -> Vec<Mark> {
        let cycle_len = self.cycle_len as f64;
        let target_frac = |t: usize| self.targets[t].at as f64 / cycle_len;
        let strokes = self.hits.iter().filter_map(|h| {
            let (cycle, frac, kind) = match h.judgment {
                Judgment::Hit {
                    cycle,
                    target,
                    error,
                } => (
                    cycle as i64,
                    target_frac(target) + error / cycle_len,
                    MarkKind::Hit(timing(self.ms(error))),
                ),
                Judgment::Extra => {
                    let pos = self.cycle_pos(h.frame);
                    (pos.floor() as i64, pos - pos.floor(), MarkKind::Extra)
                }
                Judgment::Ignored => return None,
            };
            (cycle >= from).then_some(Mark {
                voice: h.voice,
                cycle,
                frac,
                kind,
            })
        });
        // `advance` records misses in cycle order and `judge` only removes
        // them, so the ones from `from` on are a suffix.
        let first = self.misses.partition_point(|m| (m.cycle as i64) < from);
        let misses = self.misses[first..].iter().map(|m| Mark {
            voice: self.targets[m.target].voice,
            cycle: m.cycle as i64,
            frac: target_frac(m.target),
            kind: MarkKind::Miss,
        });
        strokes.chain(misses).collect()
    }

    /// The instance of `voice`'s targets nearest `frame` in the frame's
    /// cycle or either neighbour among those that can annotate an extra
    /// (`annotates`): `(cycle, target, frame − instance)`. `None` when there
    /// is none.
    fn nearest_instance(&self, frame: f64, voice: usize) -> Option<(i64, usize, f64)> {
        self.instances_near(frame, voice)
            .filter(|&(k, t, _)| self.annotates(k, t))
            .min_by(|a, b| a.2.abs().total_cmp(&b.2.abs()))
    }

    /// The raw run so far, stamped as ended now by `ended_by`.
    pub fn run(&self, ended_by: EndedBy) -> Run {
        let m = &self.meta;
        let start = self.start as f64;
        let hits = self
            .hits
            .iter()
            .map(|h| {
                let (cycle, pulse, target_tone, error_ms, judgment) = match h.judgment {
                    Judgment::Hit {
                        cycle,
                        target,
                        error,
                    } => {
                        let t = &self.targets[target];
                        (
                            cycle as i64,
                            Some(t.pulse),
                            Some(t.tone),
                            Some(self.ms(error)),
                            RunJudgment::Hit,
                        )
                    }
                    Judgment::Extra => match self.nearest_instance(h.frame, h.voice) {
                        Some((cycle, t, error)) => (
                            cycle,
                            Some(self.targets[t].pulse),
                            None,
                            Some(self.ms(error)),
                            RunJudgment::Extra,
                        ),
                        None => (self.cycle_of(h.frame), None, None, None, RunJudgment::Extra),
                    },
                    Judgment::Ignored => (
                        self.cycle_of(h.frame),
                        None,
                        None,
                        None,
                        RunJudgment::Ignored,
                    ),
                };
                RunHit {
                    t_ms: self.ms(h.frame - start),
                    voice: h.voice + 1,
                    tone: h.tone,
                    cycle,
                    pulse,
                    target_tone,
                    error_ms,
                    judgment,
                    velocity: h.velocity,
                }
            })
            .collect();
        let misses = self
            .misses
            .iter()
            .map(|m| {
                let t = &self.targets[m.target];
                RunMiss {
                    cycle: m.cycle,
                    voice: t.voice + 1,
                    pulse: t.pulse,
                }
            })
            .collect();
        Run {
            djembot: RUN_FORMAT,
            lesson: m.lesson.clone(),
            label: m.label.clone(),
            title: m.title.clone(),
            tempo: m.tempo,
            beats: m.beats,
            cycles: self.cycles,
            ending: if self.closing {
                Ending::Downbeat
            } else {
                Ending::None
            },
            count_in: m.count_in,
            window_ms: m.window_ms,
            offset_ms: m.offset_ms,
            latency_ms: m.latency_ms,
            input: m.input.clone(),
            started: m.started.clone(),
            ended: utc(SystemTime::now()),
            ended_by,
            cycles_played: self.cycles_played(),
            parts: m.parts.clone(),
            hits,
            misses,
        }
    }
}

/// The raw capture of one performance. Times are ms; voices are 1-based.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Run {
    pub djembot: u32,
    pub lesson: String,
    pub label: String,
    pub title: Option<String>,
    pub tempo: f64,
    pub beats: u32,
    pub cycles: Option<u32>,
    /// `Downbeat` when a finite block ended on a closing downbeat that its
    /// pulse-0 targets were judged against in cycle `cycles`, else `None`;
    /// format-1 and -2 files recorded no closing stroke.
    #[serde(default = "no_ending")]
    pub ending: Ending,
    pub count_in: u32,
    pub window_ms: f64,
    pub offset_ms: f64,
    /// The device-reported output latency when the performance opened,
    /// already subtracted from every stroke; absent in format-1 files and
    /// when the device had not yet reported one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latency_ms: Option<f64>,
    pub input: String,
    pub started: String,
    pub ended: String,
    pub ended_by: EndedBy,
    pub cycles_played: u32,
    pub parts: Vec<RunPart>,
    pub hits: Vec<RunHit>,
    pub misses: Vec<RunMiss>,
}

fn no_ending() -> Ending {
    Ending::None
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RunPart {
    pub voice: usize,
    pub name: String,
    pub instrument: String,
    /// Tone letter → key name.
    pub keys: BTreeMap<String, String>,
    pub pulses: u32,
    pub onsets: Vec<u32>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RunJudgment {
    Hit,
    Extra,
    Ignored,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RunHit {
    /// From the start of cycle 0, after calibration; negative before it
    /// (the count-in, or the boundary region of a block entered attacca).
    pub t_ms: f64,
    pub voice: usize,
    /// The tone struck.
    pub tone: char,
    /// The matched target's cycle; for an extra, the cycle of the nearest
    /// instance of the voice's targets that is played or lies beyond the
    /// ends of the score (-1 before cycle 0 of a block not entered attacca,
    /// `cycles` after the last block); else the cycle the stroke fell in
    /// (-1 in the count-in).
    pub cycle: i64,
    /// The matched target's pulse, or an extra's nearest one; absent for an
    /// extra on a part without onsets and in format-1 and -2 files.
    pub pulse: Option<u32>,
    /// The matched target's tone.
    pub target_tone: Option<char>,
    /// Signed distance from the matched target, or from an extra's nearest
    /// instance.
    pub error_ms: Option<f64>,
    pub judgment: RunJudgment,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub velocity: Option<f32>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunMiss {
    pub cycle: u32,
    pub voice: usize,
    pub pulse: u32,
}

impl Run {
    /// `<YYYYMMDD-HHMMSS>-<label>.json`, from `started`; characters of the
    /// label outside `[A-Za-z0-9_-]` become `_`.
    pub fn file_stem(&self) -> String {
        let stamp: String = self
            .started
            .chars()
            .filter(char::is_ascii_digit)
            .take(14)
            .collect();
        let (date, time) = stamp.split_at(stamp.len().min(8));
        let label: String = self
            .label
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        format!("{date}-{time}-{label}")
    }

    /// Writes the run as JSON into `dir` (created if missing) under a new
    /// file named by `file_stem`, adding `-2`, `-3`, … rather than
    /// overwriting. Returns the path written.
    pub fn write(&self, dir: &Path) -> io::Result<PathBuf> {
        fs::create_dir_all(dir)?;
        let json = serde_json::to_string_pretty(self).map_err(io::Error::other)?;
        let stem = self.file_stem();
        for n in 1.. {
            let name = if n == 1 {
                format!("{stem}.json")
            } else {
                format!("{stem}-{n}.json")
            };
            let path = dir.join(name);
            match fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(mut f) => {
                    f.write_all(json.as_bytes())?;
                    f.write_all(b"\n")?;
                    return Ok(path);
                }
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e),
            }
        }
        unreachable!("the name counter is unbounded")
    }
}

/// A measured tap offset, as stored in the calibration file.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Calibration {
    pub offset_ms: f64,
    pub sd_ms: f64,
    pub taps: usize,
    pub measured: String,
}

impl Calibration {
    /// The offset a calibration run measures: the median error of its
    /// matched taps. `Err(taps)` when fewer than `MIN_CALIBRATION_TAPS`
    /// matched.
    pub fn from_run(run: &Run) -> Result<Calibration, usize> {
        let errors: Vec<f64> = run.hits.iter().filter_map(|h| h.error_ms).collect();
        if errors.len() < MIN_CALIBRATION_TAPS {
            return Err(errors.len());
        }
        Ok(Calibration {
            offset_ms: median(&errors).expect("at least MIN_CALIBRATION_TAPS errors"),
            sd_ms: sample_sd(&errors).expect("at least MIN_CALIBRATION_TAPS errors"),
            taps: errors.len(),
            measured: run.ended.clone(),
        })
    }

    /// Reads the calibration file; `Ok(None)` when it does not exist.
    pub fn load(path: &Path) -> io::Result<Option<Calibration>> {
        match fs::read_to_string(path) {
            Ok(text) => serde_json::from_str(&text)
                .map(Some)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Writes the calibration file, creating its directory.
    pub fn save(&self, path: &Path) -> io::Result<()> {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        let json = serde_json::to_string_pretty(self).map_err(io::Error::other)?;
        fs::write(path, json + "\n")
    }
}

/// The median; the mean of the middle two for an even count.
pub fn median(xs: &[f64]) -> Option<f64> {
    if xs.is_empty() {
        return None;
    }
    let mut v = xs.to_vec();
    v.sort_by(f64::total_cmp);
    let n = v.len();
    Some(if n % 2 == 1 {
        v[n / 2]
    } else {
        (v[n / 2 - 1] + v[n / 2]) / 2.0
    })
}

/// The sample standard deviation; `None` below two values.
pub fn sample_sd(xs: &[f64]) -> Option<f64> {
    if xs.len() < 2 {
        return None;
    }
    let n = xs.len() as f64;
    let mean = xs.iter().sum::<f64>() / n;
    Some((xs.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / (n - 1.0)).sqrt())
}

/// An XDG base directory: `xdg` (the variable's value) when it is an
/// absolute path, else `home/fallback`.
fn xdg_dir(xdg: Option<OsString>, home: Option<OsString>, fallback: &str) -> Option<PathBuf> {
    xdg.map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| home.map(|h| PathBuf::from(h).join(fallback)))
}

/// Where run files go: `$XDG_DATA_HOME/djembot/runs`, by default
/// `~/.local/share/djembot/runs`.
// TODO(runs-in-learning-dir): let the learning directory choose.
pub fn runs_dir() -> Option<PathBuf> {
    let base = xdg_dir(
        env::var_os("XDG_DATA_HOME"),
        env::var_os("HOME"),
        ".local/share",
    );
    base.map(|d| d.join("djembot").join("runs"))
}

/// The calibration file: `$XDG_CONFIG_HOME/djembot/calibration.json`, by
/// default `~/.config/djembot/calibration.json`.
pub fn calibration_path() -> Option<PathBuf> {
    let base = xdg_dir(
        env::var_os("XDG_CONFIG_HOME"),
        env::var_os("HOME"),
        ".config",
    );
    base.map(|d| d.join("djembot").join("calibration.json"))
}

/// `t` as `YYYY-MM-DDTHH:MM:SSZ` (UTC).
pub fn utc(t: SystemTime) -> String {
    let secs = t.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs()) as i64;
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    // Civil date from days since 1970-01-01 (proleptic Gregorian).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        rem / 60 % 60,
        rem % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Diagnostics;
    use crate::sounds::Bank;
    use crate::timeline::tests::exercise;
    use serde_json::{Value, json};
    use std::sync::Arc;
    use std::time::Duration;

    const RATE: u32 = 48_000;
    /// 120 bpm at 48 kHz: one beat, and the count-in.
    const BEAT: f64 = 24_000.0;
    const CYCLE: f64 = 96_000.0;
    /// One 16th (pulse of a 16-pulse voice).
    const P16: f64 = 6_000.0;
    /// Frames per ms.
    const MS: f64 = 48.0;

    fn perf_of(v: Value, offset_ms: f64) -> Performance {
        linked(v, offset_ms, Neighbours::default())
    }

    /// A performance of `v` from frame 0 chained to `neighbours`.
    fn linked(v: Value, offset_ms: f64, neighbours: Neighbours) -> Performance {
        let ex = exercise(v);
        let mut bank = Bank::from_fn(|_| Arc::from(vec![1.0f32]));
        let tl = Timeline::from(&ex, &mut bank, RATE).unwrap();
        Performance::new(
            &ex,
            &tl,
            0,
            neighbours,
            RATE,
            conditions("/l/x.md", offset_ms, Some(9.0)),
        )
    }

    /// Two cycles of 4 beats at 120 bpm with a 4-beat count-in; voice 0 is
    /// the computer's bell, voice 1 the player's clave on pulses 0 6 8 15.
    fn clave(window_ms: f64, offset_ms: f64) -> Performance {
        perf_of(clave_json(window_ms), offset_ms)
    }

    fn clave_json(window_ms: f64) -> Value {
        json!({
            "label": "A", "title": "Clave", "tempo": 120, "beats": 4, "cycles": 2,
            "count_in": 4, "window_ms": window_ms,
            "voices": [
                { "name": "bell", "instrument": "bell", "pulses": 4, "onsets": [0, 1, 2, 3] },
                { "name": "clave", "instrument": "clave", "pulses": 16,
                  "onsets": [0, 6, 8, 15], "keys": "f" }
            ]
        })
    }

    /// `clave` with `"ending": "none"`: no closing downbeat.
    fn clave_no_ending(window_ms: f64) -> Performance {
        let mut v = clave_json(window_ms);
        v["ending"] = json!("none");
        perf_of(v, 0.0)
    }

    /// The heard frame of pulse `p` (16ths) in cycle `k`.
    fn at(k: u32, p: u32) -> f64 {
        BEAT * 4.0 + k as f64 * CYCLE + p as f64 * P16
    }

    fn hit(cycle: u32, target: usize, error: f64) -> Judgment {
        Judgment::Hit {
            cycle,
            target,
            error,
        }
    }

    #[test]
    fn targets_are_the_player_voices_only() {
        let p = clave(100.0, 0.0);
        let pulses: Vec<(usize, u32, char, u64)> = p
            .targets()
            .iter()
            .map(|t| (t.voice, t.pulse, t.tone, t.at))
            .collect();
        assert_eq!(
            pulses,
            vec![
                (1, 0, 'x', 0),
                (1, 6, 'x', 36_000),
                (1, 8, 'x', 48_000),
                (1, 15, 'x', 90_000)
            ]
        );
    }

    #[test]
    fn a_stroke_within_the_window_matches() {
        let mut p = clave(100.0, 0.0);
        assert_eq!(
            p.judge(at(0, 6) + 10.0 * MS, 1, 'x', None),
            hit(0, 1, 480.0)
        );
        assert_eq!(
            p.judge(at(0, 8) - 99.0 * MS, 1, 'x', None),
            hit(0, 2, -99.0 * MS)
        );
        assert_eq!(
            p.judge(at(1, 6) + 101.0 * MS, 1, 'x', None),
            Judgment::Extra
        );
        assert_eq!(p.tally().hit, 2);
        assert_eq!(p.tally().extra, 1);
    }

    #[test]
    fn the_nearest_target_wins() {
        // Pulses 6 and 8 are 250 ms apart; a 200 ms window reaches both.
        let mut p = clave(200.0, 0.0);
        let mid = (at(0, 6) + at(0, 8)) / 2.0;
        assert_eq!(
            p.judge(mid + 1.0, 1, 'x', None),
            hit(0, 2, 1.0 - 2.0 * P16 / 2.0)
        );
        assert_eq!(p.judge(mid - 1.0, 1, 'x', None), hit(0, 1, P16 * 1.0 - 1.0));
    }

    #[test]
    fn first_claim_wins_and_a_second_stroke_is_extra() {
        let mut p = clave(100.0, 0.0);
        assert_eq!(
            p.judge(at(0, 6) + 50.0 * MS, 1, 'x', None),
            hit(0, 1, 50.0 * MS)
        );
        assert_eq!(p.judge(at(0, 6) + 2.0 * MS, 1, 'x', None), Judgment::Extra);
        let judged: Vec<Judgment> = p.hits().iter().map(|h| h.judgment).collect();
        assert_eq!(judged, vec![hit(0, 1, 50.0 * MS), Judgment::Extra]);
        let t = p.tally();
        assert_eq!((t.hit, t.extra), (1, 1));
        assert_eq!(t.mean_ms, Some(50.0));
    }

    #[test]
    fn other_voices_strokes_do_not_match_this_voice() {
        let mut p = clave(100.0, 0.0);
        assert_eq!(p.judge(at(0, 6), 0, 'h', None), Judgment::Extra);
    }

    #[test]
    fn early_on_late_at_twenty_ms() {
        assert_eq!(timing(-20.0), Timing::On);
        assert_eq!(timing(20.0), Timing::On);
        assert_eq!(timing(0.0), Timing::On);
        assert_eq!(timing(-20.1), Timing::Early);
        assert_eq!(timing(20.1), Timing::Late);
    }

    #[test]
    fn marks_place_hits_extras_and_misses_in_their_cycle() {
        let mut p = clave(100.0, 0.0);
        p.judge(at(0, 0) - 2.0 * BEAT, 1, 'x', None); // count-in: ignored
        p.judge(at(0, 6) + 30.0 * MS, 1, 'x', None);
        p.judge(at(0, 8) - 30.0 * MS, 1, 'x', None);
        p.judge(at(0, 3), 1, 'x', None);
        p.advance(at(0, 15) + 101.0 * MS);
        p.judge(at(1, 0) - 10.0 * MS, 1, 'x', None);
        let marks: Vec<(usize, i64, i64, MarkKind)> = p
            .marks_since(0)
            .iter()
            .map(|m| (m.voice, m.cycle, (m.frac * 1e6).round() as i64, m.kind))
            .collect();
        assert_eq!(
            marks,
            vec![
                (1, 0, 390_000, MarkKind::Hit(Timing::Late)),
                (1, 0, 485_000, MarkKind::Hit(Timing::Early)),
                (1, 0, 187_500, MarkKind::Extra),
                (1, 1, -5_000, MarkKind::Hit(Timing::On)),
                (1, 0, 0, MarkKind::Miss),
                (1, 0, 937_500, MarkKind::Miss),
            ]
        );
    }

    #[test]
    fn marks_since_drops_earlier_cycles() {
        let mut p = clave(100.0, 0.0);
        p.judge(at(0, 6), 1, 'x', None);
        p.judge(at(0, 3), 1, 'x', None);
        p.judge(at(1, 0) - 10.0 * MS, 1, 'x', None);
        p.judge(at(1, 3), 1, 'x', None);
        p.advance(at(2, 15) + 101.0 * MS);
        let all = p.marks_since(0);
        let later = p.marks_since(1);
        assert!(all.iter().any(|m| m.cycle == 0));
        assert!(later.iter().any(|m| m.kind == MarkKind::Miss));
        assert!(later.iter().any(|m| m.kind == MarkKind::Extra));
        let expected: Vec<Mark> = all.into_iter().filter(|m| m.cycle >= 1).collect();
        assert_eq!(later, expected);
        assert_eq!(p.marks_since(3), vec![]);
    }

    #[test]
    fn the_current_cycle_follows_the_heard_clock_and_stops_at_the_last() {
        let mut p = clave(100.0, 0.0);
        assert_eq!(p.current_cycle(), 0);
        p.advance(at(0, 0) - BEAT);
        assert_eq!(p.current_cycle(), 0, "count-in");
        p.advance(at(1, 3));
        assert_eq!(p.current_cycle(), 1);
        p.finish();
        assert_eq!(p.current_cycle(), 1, "two cycles, 0-based");
    }

    #[test]
    fn a_miss_appears_when_its_window_closes() {
        let mut p = clave(100.0, 0.0);
        let close = at(0, 0) + 100.0 * MS;
        p.advance(close);
        assert!(p.misses().is_empty());
        p.advance(close + 1.0);
        assert_eq!(
            p.misses(),
            &[MissRecord {
                cycle: 0,
                target: 0
            }]
        );
        p.advance(close + 2.0);
        assert_eq!(p.misses().len(), 1, "a miss is recorded once");
        p.advance(at(1, 0) + 101.0 * MS);
        let ms: Vec<(u32, u32)> = p
            .misses()
            .iter()
            .map(|m| (m.cycle, p.targets()[m.target].pulse))
            .collect();
        assert_eq!(ms, vec![(0, 0), (0, 6), (0, 8), (0, 15), (1, 0)]);
    }

    #[test]
    fn a_claimed_target_is_never_missed() {
        let mut p = clave(100.0, 0.0);
        p.judge(at(0, 0) + MS, 1, 'x', None);
        p.advance(at(0, 6));
        assert!(p.misses().is_empty());
    }

    #[test]
    fn a_stroke_drained_after_its_window_closed_withdraws_the_miss() {
        let mut p = clave(100.0, 0.0);
        p.advance(at(0, 6) + 101.0 * MS);
        assert_eq!(p.misses().len(), 2);
        assert_eq!(
            p.judge(at(0, 6) + 99.0 * MS, 1, 'x', None),
            hit(0, 1, 99.0 * MS)
        );
        assert_eq!(
            p.misses(),
            &[MissRecord {
                cycle: 0,
                target: 0
            }]
        );
    }

    #[test]
    fn a_stroke_before_a_cycle_boundary_matches_the_next_pulse_zero() {
        let mut p = clave(100.0, 0.0);
        assert_eq!(
            p.judge(at(1, 0) - 30.0 * MS, 1, 'x', None),
            hit(1, 0, -30.0 * MS)
        );
    }

    #[test]
    fn a_count_in_stroke_can_hit_cycle_zero_pulse_zero() {
        let mut p = clave(100.0, 0.0);
        assert_eq!(
            p.judge(at(0, 0) - 50.0 * MS, 1, 'x', None),
            hit(0, 0, -50.0 * MS)
        );
        assert_eq!(p.run(EndedBy::Open).hits[0].cycle, 0);
    }

    #[test]
    fn count_in_strokes_outside_a_window_are_ignored_not_extra() {
        let mut p = clave(100.0, 0.0);
        assert_eq!(p.judge(BEAT, 1, 'x', None), Judgment::Ignored);
        assert_eq!(
            p.judge(at(0, 0) - 101.0 * MS, 1, 'x', None),
            Judgment::Ignored
        );
        // Within the window of pulse 0 but it is already claimed.
        p.judge(at(0, 0), 1, 'x', None);
        assert_eq!(
            p.judge(at(0, 0) - 10.0 * MS, 1, 'x', None),
            Judgment::Ignored
        );
        let run = p.run(EndedBy::Open);
        assert_eq!(run.hits[0].cycle, -1);
        assert_eq!(run.hits[0].t_ms, -1500.0);
        assert_eq!(p.tally().extra, 0);
    }

    #[test]
    fn a_count_in_stroke_on_a_voice_without_pulse_zero_is_ignored() {
        let mut p = perf_of(
            json!({ "tempo": 120, "beats": 4, "count_in": 4,
                    "voices": [{ "name": "c", "instrument": "clave", "pulses": 16,
                                 "onsets": [2], "keys": "f" }] }),
            0.0,
        );
        assert_eq!(
            p.judge(at(0, 0) - 10.0 * MS, 0, 'x', None),
            Judgment::Ignored
        );
        assert_eq!(p.judge(at(0, 0) + 10.0 * MS, 0, 'x', None), Judgment::Extra);
    }

    #[test]
    fn after_end_only_the_last_cycle_matches() {
        // 200 ms window: pulse 15 of the last cycle is 125 ms before `end`;
        // the next pulse 0 would be nearer, but without a closing downbeat
        // it does not exist.
        let mut p = clave_no_ending(200.0);
        let end = at(2, 0);
        assert_eq!(
            p.judge(end + 50.0 * MS, 1, 'x', None),
            hit(1, 3, 175.0 * MS)
        );
        assert_eq!(p.judge(end + 100.0 * MS, 1, 'x', None), Judgment::Extra);
        assert_eq!(p.judge(end + 201.0 * MS, 1, 'x', None), Judgment::Ignored);
        assert_eq!(p.tally().extra, 1);
    }

    #[test]
    fn closing_target_matches_the_final_tap() {
        let end = at(2, 0);
        // Pulse 15 of the last cycle is 125 ms before `end`.
        for error_ms in [-50.0, 0.0, 90.0] {
            let mut p = clave(100.0, 0.0);
            assert_eq!(
                p.judge(end + error_ms * MS, 1, 'x', None),
                hit(2, 0, error_ms * MS),
                "{error_ms}"
            );
            p.finish();
            let run = p.run(EndedBy::End);
            assert_eq!(run.ending, Ending::Downbeat);
            assert_eq!((run.hits[0].cycle, run.hits[0].pulse), (2, Some(0)));
            assert!(!run.misses.iter().any(|m| m.cycle == 2));
            assert_eq!(p.cycles_played(), 2);
            assert_eq!(p.current_cycle(), 1);
        }
        let mut p = clave(100.0, 0.0);
        assert_eq!(p.judge(end + 101.0 * MS, 1, 'x', None), Judgment::Ignored);
    }

    #[test]
    fn closing_target_is_missed_when_untapped() {
        let mut p = clave(100.0, 0.0);
        let end = at(2, 0);
        p.advance(end + 99.0 * MS);
        assert!(!p.misses().iter().any(|m| m.cycle == 2));
        p.advance(end + 101.0 * MS);
        let closing: Vec<_> = p.misses().iter().filter(|m| m.cycle == 2).collect();
        assert_eq!(
            closing,
            [&MissRecord {
                cycle: 2,
                target: 0
            }]
        );
        p.finish();
        assert_eq!(p.misses().iter().filter(|m| m.cycle == 2).count(), 1);
    }

    #[test]
    fn no_closing_target_for_ending_none() {
        let mut p = clave_no_ending(100.0);
        let end = at(2, 0);
        assert_eq!(p.judge(end, 1, 'x', None), Judgment::Extra);
        p.finish();
        assert_eq!(p.misses().len(), 8);
        let run = p.run(EndedBy::End);
        assert_eq!(run.ending, Ending::None);
        // The extra is annotated against the instance that is not played.
        assert_eq!(
            (run.hits[0].cycle, run.hits[0].pulse, run.hits[0].error_ms),
            (2, Some(0), Some(0.0))
        );
    }

    #[test]
    fn no_closing_target_without_pulse_zero() {
        let mut p = perf_of(
            json!({ "tempo": 120, "beats": 4, "cycles": 1, "count_in": 4,
                    "voices": [{ "name": "c", "instrument": "clave", "pulses": 16,
                                 "onsets": [2, 8], "keys": "f" }] }),
            0.0,
        );
        assert_eq!(p.judge(at(1, 0), 0, 'x', None), Judgment::Extra);
        p.finish();
        assert_eq!(p.misses().len(), 2);
        // The block still ends on a closing click.
        assert_eq!(p.run(EndedBy::End).ending, Ending::Downbeat);
    }

    /// A recorded run: 60 bpm, 4 beats, window 100 ms,
    /// a part on every beat. Strokes just outside the window carry the
    /// beat they were aimed at.
    #[test]
    fn extras_carry_their_nearest_instance() {
        let beat = RATE as f64;
        let mut p = perf_of(
            json!({ "tempo": 60, "beats": 4, "cycles": 8, "count_in": 4, "window_ms": 100,
                    "ending": "none",
                    "voices": [{ "name": "f", "instrument": "click", "pulses": 4,
                                 "onsets": [0, 1, 2, 3], "keys": "f" }] }),
            0.0,
        );
        let start = 4.0 * beat;
        let cycle = 4.0 * beat;
        // −114.9 ms from cycle 3 pulse 0: the stroke lies in cycle 2.
        let early = start + 3.0 * cycle - 114.9 * MS;
        assert_eq!(p.judge(early, 0, 'x', None), Judgment::Extra);
        // +100.7 ms from cycle 4 pulse 2.
        let late = start + 4.0 * cycle + 2.0 * beat + 100.7 * MS;
        assert_eq!(p.judge(late, 0, 'x', None), Judgment::Extra);
        // The final tap, −12.8 ms from the "one" after cycle 8.
        let last = start + 8.0 * cycle - 12.8 * MS;
        assert_eq!(p.judge(last, 0, 'x', None), Judgment::Extra);
        let run = p.run(EndedBy::End);
        let got: Vec<(i64, Option<u32>, Option<char>, f64)> = run
            .hits
            .iter()
            .map(|h| (h.cycle, h.pulse, h.target_tone, h.error_ms.unwrap()))
            .collect();
        let want = [(3, 0, -114.9), (4, 2, 100.7), (8, 0, -12.8)];
        assert_eq!(got.len(), want.len());
        for (g, w) in got.iter().zip(want) {
            assert_eq!((g.0, g.1, g.2), (w.0, Some(w.1), None), "{g:?}");
            assert!((g.3 - w.2).abs() < 1e-9, "{g:?}");
        }
        assert!(run.hits.iter().all(|h| h.judgment == RunJudgment::Extra));
    }

    #[test]
    fn an_extra_on_a_part_without_onsets_stays_unannotated() {
        let mut p = perf_of(
            json!({ "tempo": 120, "beats": 4,
                    "voices": [{ "name": "c", "instrument": "clave", "pulses": 4,
                                 "onsets": [], "keys": "f" }] }),
            0.0,
        );
        assert_eq!(p.judge(BEAT, 0, 'x', None), Judgment::Extra);
        let h = &p.run(EndedBy::Stop).hits[0];
        assert_eq!((h.cycle, h.pulse, h.error_ms), (0, None, None));
    }

    #[test]
    fn format_2_run_reads_with_ending_none() {
        let mut v = serde_json::to_value(clave(100.0, 0.0).run(EndedBy::End)).unwrap();
        v["djembot"] = json!(2);
        v.as_object_mut().unwrap().remove("ending");
        let run: Run = serde_json::from_value(v).unwrap();
        assert_eq!(run.ending, Ending::None);
    }

    #[test]
    fn a_run_with_an_unknown_ending_is_rejected() {
        let mut v = serde_json::to_value(clave(100.0, 0.0).run(EndedBy::End)).unwrap();
        v["ending"] = json!("cued");
        assert!(serde_json::from_value::<Run>(v).is_err());
    }

    #[test]
    fn finish_misses_every_unclaimed_instance() {
        let mut p = clave(100.0, 0.0);
        p.judge(at(0, 8), 1, 'x', None);
        p.judge(at(1, 15), 1, 'x', None);
        p.advance(at(0, 9));
        p.finish();
        // Two cycles of four targets and the closing pulse 0, two hit.
        assert_eq!(p.misses().len(), 8 + 1 - 2);
        assert_eq!(p.cycles_played(), 2);
        p.finish();
        assert_eq!(p.misses().len(), 7);
    }

    #[test]
    fn finish_on_a_looping_block_does_nothing() {
        let mut p = perf_of(
            json!({ "tempo": 120, "beats": 4,
                    "voices": [{ "name": "c", "instrument": "clave", "pulses": 4,
                                 "onsets": [0], "keys": "f" }] }),
            0.0,
        );
        p.advance(2.5 * CYCLE);
        assert_eq!(p.misses().len(), 3);
        p.finish();
        assert_eq!(p.misses().len(), 3);
        assert_eq!(p.cycles_played(), 3);
    }

    #[test]
    fn cycles_played_counts_cycles_begun() {
        let mut p = clave(100.0, 0.0);
        assert_eq!(p.cycles_played(), 0);
        p.advance(BEAT);
        assert_eq!(p.cycles_played(), 0);
        p.advance(at(0, 1));
        assert_eq!(p.cycles_played(), 1);
        p.advance(at(1, 1));
        assert_eq!(p.cycles_played(), 2);
        p.advance(at(5, 0));
        assert_eq!(p.cycles_played(), 2);
    }

    #[test]
    fn candidate_reports_the_nearest_unclaimed_distance() {
        let mut p = clave(100.0, 0.0);
        let stroke = at(0, 6) + 30.0 * MS;
        assert_eq!(p.candidate(stroke, 1), Some(30.0 * MS));
        assert_eq!(p.candidate(stroke, 0), None, "not a player voice");
        assert_eq!(
            p.candidate(at(0, 6) + 101.0 * MS, 1),
            None,
            "outside the window"
        );
        assert!(p.hits().is_empty(), "nothing judged");
        p.judge(at(0, 6) - 5.0 * MS, 1, 'x', None);
        assert_eq!(p.candidate(stroke, 1), None, "the nearest is claimed");
        assert_eq!(p.judge(stroke, 1, 'x', None), Judgment::Extra);
        // The closing downbeat after cycle 1, and nothing past its window.
        assert_eq!(p.candidate(at(2, 0) - 20.0 * MS, 1), Some(20.0 * MS));
        assert_eq!(p.candidate(at(2, 0) + 101.0 * MS, 1), None);
        // Before the first window.
        assert_eq!(p.candidate(at(0, 0) - 101.0 * MS, 1), None);
    }

    #[test]
    fn candidate_follows_the_calibration_offset() {
        let p = clave(100.0, -45.0);
        // Heard 60 ms early, judged 15 ms early.
        assert_eq!(p.candidate(at(0, 8) - 60.0 * MS, 1), Some(15.0 * MS));
    }

    #[test]
    fn origin_shifts_every_instance() {
        let ex = exercise(clave_json(100.0));
        let mut bank = Bank::from_fn(|_| Arc::from(vec![1.0f32]));
        let tl = Timeline::from(&ex, &mut bank, RATE).unwrap();
        const ORIGIN: u64 = 384_000;
        let o = ORIGIN as f64;
        let mut p = Performance::new(
            &ex,
            &tl,
            ORIGIN,
            Neighbours::default(),
            RATE,
            conditions("/l/x.md", 0.0, None),
        );
        assert_eq!(p.start(), ORIGIN + 4 * BEAT as u64);
        assert_eq!(p.end(), Some(ORIGIN + 4 * BEAT as u64 + 2 * CYCLE as u64));
        assert_eq!(p.instance(1, 1), o + at(1, 6));
        assert_eq!(
            p.judge(at(0, 0), 1, 'x', None),
            Judgment::Ignored,
            "before the origin"
        );
        assert_eq!(
            p.judge(o + at(0, 6) + 10.0 * MS, 1, 'x', None),
            hit(0, 1, 10.0 * MS)
        );
        assert_eq!(
            p.judge(o + at(1, 15) - 3.0 * MS, 1, 'x', None),
            hit(1, 3, -3.0 * MS)
        );
        p.advance(o + at(1, 1));
        assert_eq!(p.cycles_played(), 2);
        assert_eq!(p.current_cycle(), 1);
        let missed: Vec<(u32, usize)> = p.misses().iter().map(|m| (m.cycle, m.target)).collect();
        assert_eq!(missed, [(0, 0), (0, 2), (0, 3), (1, 0)]);
        assert!(!p.closed(o + at(2, 0) + 100.0 * MS));
        assert!(p.closed(o + at(2, 0) + 101.0 * MS));
        let run = p.run(EndedBy::End);
        assert_eq!(run.hits[1].t_ms, (at(0, 6) + 10.0 * MS - 4.0 * BEAT) / MS);
        assert_eq!(run.hits[0].cycle, -4, "a stroke before the block");
    }

    /// Two cycles of 4 beats at 120 bpm, no count-in, the player's clave
    /// on `onsets`; `next` is `"attacca"` or `"stop"`.
    fn link_json(onsets: &[u32], next: &str) -> Value {
        json!({
            "tempo": 120, "beats": 4, "cycles": 2, "next": next,
            "voices": [{ "name": "clave", "instrument": "clave", "pulses": 16,
                         "onsets": onsets, "keys": "f" }]
        })
    }

    /// Entered attacca from a link with a beat of `BEAT`.
    const ENTERED: Neighbours = Neighbours {
        before: Some(BEAT as u64),
        after: None,
    };

    /// Followed attacca by a link with a beat of `BEAT`.
    const FOLLOWED: Neighbours = Neighbours {
        before: None,
        after: Some(BEAT as u64),
    };

    /// `(cycle, pulse, error_ms, judgment)` of each recorded stroke.
    fn annotations(p: &Performance) -> Vec<(i64, Option<u32>, Option<f64>, RunJudgment)> {
        p.run(EndedBy::Open)
            .hits
            .iter()
            .map(|h| (h.cycle, h.pulse, h.error_ms.map(f64::round), h.judgment))
            .collect()
    }

    #[test]
    fn an_attacca_link_judges_from_a_beat_before_its_origin() {
        let early = -150.0 * MS;
        let mut b = linked(link_json(&[0, 6], "stop"), 0.0, ENTERED);
        assert_eq!(span_start(0, 4_800, ENTERED.before), -BEAT);
        assert_eq!(b.judge(early, 0, 'x', None), Judgment::Extra);
        assert_eq!(b.judge(-BEAT, 0, 'x', None), Judgment::Extra);
        assert_eq!(b.judge(-BEAT - 1.0, 0, 'x', None), Judgment::Ignored);
        let got = annotations(&b);
        assert_eq!(got[0], (0, Some(0), Some(-150.0), RunJudgment::Extra));
        assert_eq!(b.run(EndedBy::Open).hits[0].t_ms, -150.0);
        // The same stroke on a block played on its own is in its count-in.
        let mut head = perf_of(link_json(&[0, 6], "stop"), 0.0);
        assert_eq!(head.judge(early, 0, 'x', None), Judgment::Ignored);
    }

    #[test]
    fn a_link_with_a_successor_closes_a_beat_after_its_end() {
        let mut a = linked(link_json(&[0, 15], "attacca"), 0.0, FOLLOWED);
        let end = 2.0 * CYCLE;
        assert_eq!(a.end(), Some(end as u64));
        assert!(!a.closed(end + BEAT));
        assert!(a.closed(end + BEAT + 1.0));
        // 30 ms after the end, 155 ms after the last target: the "one"
        // after the end is the next block's, not this one's.
        assert_eq!(a.judge(end + 30.0 * MS, 0, 'x', None), Judgment::Extra);
        assert_eq!(a.judge(end + BEAT, 0, 'x', None), Judgment::Extra);
        assert_eq!(a.judge(end + BEAT + 1.0, 0, 'x', None), Judgment::Ignored);
        assert_eq!(
            annotations(&a)[0],
            (1, Some(15), Some(155.0), RunJudgment::Extra)
        );
    }

    #[test]
    fn the_window_widens_the_region_when_it_is_longer_than_a_beat() {
        // 300 bpm: a beat is 200 ms (9 600 frames), shorter than the window.
        let beat = 9_600;
        let window = 500.0 * MS;
        let mut v = link_json(&[0, 6], "attacca");
        v["tempo"] = json!(300);
        v["window_ms"] = json!(500);
        let both = Neighbours {
            before: Some(beat),
            after: Some(beat),
        };
        let mut p = linked(v, 0.0, both);
        assert_eq!(span_start(0, window as u64, both.before), -window);
        let end = p.end().unwrap() as f64;
        assert!(!p.closed(end + window));
        assert!(p.closed(end + window + 1.0));
        assert_eq!(p.judge(-window - 1.0, 0, 'x', None), Judgment::Ignored);
        assert_eq!(p.judge(-window, 0, 'x', None), hit(0, 0, -window));
    }

    #[test]
    fn virtual_instances_only_beyond_the_score() {
        // Pulse 12 of cycle −1 is nearer pulse 2 of cycle 0 than pulse 12
        // of cycle 0 is.
        let first = 2.0 * P16;
        let mut head = perf_of(link_json(&[12], "stop"), 0.0);
        head.judge(first, 0, 'x', None);
        assert_eq!(
            annotations(&head)[0],
            (-1, Some(12), Some(750.0), RunJudgment::Extra)
        );
        let mut entered = linked(link_json(&[12], "stop"), 0.0, ENTERED);
        entered.judge(first, 0, 'x', None);
        assert_eq!(
            annotations(&entered)[0],
            (0, Some(12), Some(-1_250.0), RunJudgment::Extra)
        );
        // Pulse 2 after the last cycle is nearer pulse 15 of the last cycle
        // than pulse 2 of that cycle is.
        let last = 2.0 * CYCLE - P16;
        let mut followed = linked(link_json(&[2], "attacca"), 0.0, FOLLOWED);
        followed.judge(last, 0, 'x', None);
        assert_eq!(
            annotations(&followed)[0],
            (1, Some(2), Some(1_625.0), RunJudgment::Extra)
        );
        let mut alone = perf_of(link_json(&[2], "stop"), 0.0);
        alone.judge(last, 0, 'x', None);
        assert_eq!(
            annotations(&alone)[0],
            (2, Some(2), Some(-375.0), RunJudgment::Extra)
        );
    }

    #[test]
    fn nearest_reports_the_nearest_existing_instance_at_any_distance() {
        let mut p = clave(100.0, 0.0);
        let stroke = at(0, 6) + 30.0 * MS;
        assert_eq!(p.nearest(stroke, 1), Some(30.0 * MS));
        p.judge(at(0, 6), 1, 'x', None);
        assert_eq!(p.nearest(stroke, 1), Some(30.0 * MS), "claimed or not");
        assert_eq!(p.nearest(at(0, 3), 1), Some(3.0 * P16), "beyond the window");
        assert_eq!(p.nearest(stroke, 0), None, "not a player voice");
        // Pulse 15 of cycle −1 would be 75 ms away; it is not played.
        assert_eq!(p.nearest(at(0, 0) - 50.0 * MS, 1), Some(50.0 * MS));
        assert_eq!(p.nearest(at(0, 0) - 101.0 * MS, 1), None, "before the span");
        // The closing downbeat exists.
        assert_eq!(p.nearest(at(2, 0) - 20.0 * MS, 1), Some(20.0 * MS));
        assert_eq!(p.nearest(at(2, 0) + 101.0 * MS, 1), None, "after the span");
    }

    #[test]
    fn a_wrong_tone_is_a_hit_with_both_tones_recorded() {
        let mut p = perf_of(
            json!({ "tempo": 120, "beats": 4,
                    "voices": [{ "name": "bell", "instrument": "bell", "pulses": 4,
                                 "onsets": [0, {"at": 1, "tone": "l"}],
                                 "keys": {"h": "f", "l": "d"} }] }),
            0.0,
        );
        assert_eq!(p.judge(BEAT, 0, 'h', None), hit(0, 1, 0.0));
        let run = p.run(EndedBy::Open);
        assert_eq!(run.hits[0].tone, 'h');
        assert_eq!(run.hits[0].target_tone, Some('l'));
        assert_eq!(run.parts[0].keys["l"], "d");
    }

    #[test]
    fn the_calibration_offset_shifts_errors() {
        let mut p = clave(100.0, 12.0);
        assert_eq!(p.judge(at(0, 6) + 12.0 * MS, 1, 'x', None), hit(0, 1, 0.0));
        assert_eq!(
            p.judge(at(0, 8) + 2.0 * MS, 1, 'x', None),
            hit(0, 2, -10.0 * MS)
        );
        let run = p.run(EndedBy::Stop);
        assert_eq!(run.offset_ms, 12.0);
        assert_eq!(run.hits[1].t_ms, 8.0 * 125.0 - 10.0);
    }

    /// Misses and the end of a finite block are on the calibrated clock too:
    /// a user who taps 12 ms late keeps every window 12 ms longer.
    #[test]
    fn the_calibration_offset_delays_misses_and_the_close() {
        let mut p = clave(100.0, 12.0);
        let first = at(0, 0);
        p.advance(first + 100.0 * MS + 6.0 * MS);
        assert!(
            p.misses().is_empty(),
            "window not closed on the calibrated clock"
        );
        p.advance(first + 100.0 * MS + 13.0 * MS);
        assert_eq!(p.misses().len(), 1);
        let end = p.end().unwrap() as f64;
        assert!(!p.closed(end + 100.0 * MS + 11.0 * MS));
        assert!(p.closed(end + 100.0 * MS + 13.0 * MS));
        assert!(!clave(100.0, 0.0).closed(end + 100.0 * MS));
    }

    #[test]
    fn run_serialises_in_ms_relative_to_cycle_zero_and_round_trips() {
        let mut p = clave(100.0, 0.0);
        p.judge(at(0, 6) - 14.0 * MS, 1, 'x', Some(0.5));
        p.judge(at(0, 6) + 30.0 * MS, 1, 'x', None);
        p.advance(at(1, 0) - 1.0);
        let run = p.run(EndedBy::Stop);
        let v = serde_json::to_value(&run).unwrap();
        assert_eq!(v["djembot"], 3);
        assert_eq!(v["ending"], "downbeat");
        assert_eq!(v["lesson"], "/l/x.md");
        assert_eq!(v["label"], "A");
        assert_eq!(v["title"], "Clave");
        assert_eq!(v["tempo"], 120.0);
        assert_eq!(v["cycles"], 2);
        assert_eq!(v["count_in"], 4);
        assert_eq!(v["window_ms"], 100.0);
        assert_eq!(v["latency_ms"], 9.0);
        assert_eq!(v["input"], "1 device");
        assert_eq!(v["ended_by"], "stop");
        assert_eq!(v["cycles_played"], 1);
        assert_eq!(
            v["parts"],
            json!([{ "voice": 2, "name": "clave", "instrument": "clave",
                     "keys": {"x": "f"}, "pulses": 16, "onsets": [0, 6, 8, 15] }])
        );
        assert_eq!(
            v["hits"][0],
            json!({ "t_ms": 736.0, "voice": 2, "tone": "x", "cycle": 0, "pulse": 6,
                    "target_tone": "x", "error_ms": -14.0, "judgment": "hit", "velocity": 0.5 })
        );
        assert_eq!(
            v["hits"][1],
            json!({ "t_ms": 780.0, "voice": 2, "tone": "x", "cycle": 0, "pulse": 6,
                    "target_tone": null, "error_ms": 30.0, "judgment": "extra" })
        );
        assert_eq!(
            v["misses"],
            json!([{ "cycle": 0, "voice": 2, "pulse": 0 }, { "cycle": 0, "voice": 2, "pulse": 8 },
                   { "cycle": 0, "voice": 2, "pulse": 15 }])
        );
        let text = serde_json::to_string(&run).unwrap();
        assert_eq!(serde_json::from_str::<Run>(&text).unwrap(), run);
    }

    #[test]
    fn a_format_1_run_reads_without_a_latency() {
        let run = clave(100.0, 0.0).run(EndedBy::Stop);
        let mut v = serde_json::to_value(&run).unwrap();
        v["djembot"] = json!(1);
        v.as_object_mut().unwrap().remove("latency_ms");
        let back: Run = serde_json::from_value(v).unwrap();
        assert_eq!((back.djembot, back.latency_ms), (1, None));
    }

    #[test]
    fn run_files_are_named_by_start_and_label_and_never_overwritten() {
        let mut run = clave(100.0, 0.0).run(EndedBy::End);
        run.started = "2026-10-11T10:12:03Z".into();
        run.label = "son clave/2".into();
        assert_eq!(run.file_stem(), "20261011-101203-son_clave_2");
        let dir = std::env::temp_dir().join(format!("djembot-perf-{}-runs", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let a = run.write(&dir.join("runs")).unwrap();
        let b = run.write(&dir.join("runs")).unwrap();
        assert_eq!(a.file_name().unwrap(), "20261011-101203-son_clave_2.json");
        assert_eq!(b.file_name().unwrap(), "20261011-101203-son_clave_2-2.json");
        let back: Run = serde_json::from_str(&fs::read_to_string(&a).unwrap()).unwrap();
        assert_eq!(back, run);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn xdg_dirs_prefer_an_absolute_variable_over_home() {
        let os = |s: &str| Some(OsString::from(s));
        assert_eq!(
            xdg_dir(os("/x"), os("/h"), ".config"),
            Some(PathBuf::from("/x"))
        );
        assert_eq!(
            xdg_dir(os("rel"), os("/h"), ".config"),
            Some(PathBuf::from("/h/.config"))
        );
        assert_eq!(
            xdg_dir(None, os("/h"), ".local/share"),
            Some(PathBuf::from("/h/.local/share"))
        );
        assert_eq!(xdg_dir(None, None, ".config"), None);
        if let Some(d) = runs_dir() {
            assert!(d.ends_with("djembot/runs"));
        }
        if let Some(p) = calibration_path() {
            assert!(p.ends_with("djembot/calibration.json"));
        }
    }

    #[test]
    fn utc_formats_civil_dates() {
        assert_eq!(utc(UNIX_EPOCH), "1970-01-01T00:00:00Z");
        assert_eq!(
            utc(UNIX_EPOCH + Duration::from_secs(951_782_400)),
            "2000-02-29T00:00:00Z"
        );
        assert_eq!(
            utc(UNIX_EPOCH + Duration::from_secs(1_791_713_523)),
            "2026-10-11T10:12:03Z"
        );
    }

    #[test]
    fn median_and_sd() {
        assert_eq!(median(&[]), None);
        assert_eq!(median(&[3.0, 1.0, 2.0]), Some(2.0));
        assert_eq!(median(&[4.0, 1.0, 3.0, 2.0]), Some(2.5));
        assert_eq!(sample_sd(&[1.0]), None);
        assert_eq!(
            sample_sd(&[2.0, 4.0, 4.0, 4.0, 5.0, 5.0, 7.0, 9.0]),
            Some((32.0f64 / 7.0).sqrt())
        );
    }

    fn calibration_perf() -> Performance {
        let mut diag = Diagnostics::default();
        let lesson = crate::lesson::parse(CALIBRATION_LESSON, Path::new("/"), &mut diag).unwrap();
        assert!(
            diag.errors.is_empty() && diag.warnings.is_empty(),
            "{diag:?}"
        );
        let ex = lesson.blocks().next().unwrap().clone();
        let mut bank = Bank::from_fn(|_| Arc::from(vec![1.0f32]));
        let tl = Timeline::from(&ex, &mut bank, RATE).unwrap();
        Performance::new(
            &ex,
            &tl,
            0,
            Neighbours::default(),
            RATE,
            conditions(CALIBRATION_NAME, 0.0, None),
        )
    }

    #[test]
    fn ended_by_names_are_the_run_file_spelling() {
        use EndedBy::*;
        for e in [Stop, Restart, Show, Switch, Quit, End, Enough, Open] {
            assert_eq!(serde_json::to_value(e).unwrap(), e.as_str());
        }
        assert_eq!(Enough.as_str(), "enough");
    }

    #[test]
    fn the_calibration_lesson_is_one_looping_silent_click_block_on_f() {
        let mut diag = Diagnostics::default();
        let lesson = crate::lesson::parse(CALIBRATION_LESSON, Path::new("/"), &mut diag).unwrap();
        assert_eq!(lesson.labels(), vec!["cal"]);
        let ex = lesson.blocks().next().unwrap();
        assert_eq!(
            (ex.tempo, ex.beats, ex.cycles, ex.count_in),
            (100.0, 4, None, 0),
            "starter/instructor.md *Calibration*'s tempo check compares its 160 bpm against this 100 bpm"
        );
        assert_eq!(ex.window_ms(), 300.0, "half the 600 ms beat");
        let v = &ex.voices[0];
        assert!(!v.mute && v.silent);
        assert_eq!(v.keys[0].key, "f");
        assert_eq!(
            v.onsets.iter().map(|o| o.at).collect::<Vec<_>>(),
            vec![0, 1, 2, 3]
        );
        assert_eq!(calibration_perf().targets().len(), 4);
    }

    /// A calibration run with `n` taps, tap `i` at error `err(i)` ms on
    /// click `i`, stopped once the window of click `n` has closed.
    fn calibration_run(n: u32, err: impl Fn(u32) -> f64) -> Run {
        let mut p = calibration_perf();
        // 100 bpm at 48 kHz: 28 800 frames a beat, no count-in, a 300 ms
        // window.
        for i in 0..n {
            p.judge(i as f64 * 28_800.0 + err(i) * MS, 0, 'x', None);
        }
        p.advance(n as f64 * 28_800.0 + 300.0 * MS + 1.0);
        p.run(EndedBy::Enough)
    }

    #[test]
    fn the_calibration_window_takes_a_lead_of_almost_half_a_beat() {
        let mut p = calibration_perf();
        let beat = 28_800.0;
        assert!(matches!(
            p.judge(beat - 290.0 * MS, 0, 'x', None),
            Judgment::Hit {
                cycle: 0,
                target: 1,
                ..
            }
        ));
        assert_eq!(
            p.judge(-310.0 * MS, 0, 'x', None),
            Judgment::Ignored,
            "more than a window before the first click"
        );
        assert!(matches!(
            p.judge(-290.0 * MS, 0, 'x', None),
            Judgment::Hit {
                cycle: 0,
                target: 0,
                ..
            }
        ));
    }

    #[test]
    fn calibration_counts_taps_and_takes_their_median() {
        let mut p = calibration_perf();
        assert!(p.is_calibration());
        assert_eq!((p.hit_count(), p.median_ms()), (0, None));
        for (i, e) in [-120.0, -100.0, -112.0].into_iter().enumerate() {
            p.judge(i as f64 * 28_800.0 + e * MS, 0, 'x', None);
        }
        p.judge(2.0 * 28_800.0, 0, 'x', None);
        assert_eq!(p.hit_count(), 3, "a second tap on a claimed click is extra");
        assert_eq!(p.median_ms().map(f64::round), Some(-112.0));
    }

    #[test]
    fn calibration_takes_the_median_with_enough_taps() {
        let run = calibration_run(31, |i| {
            if i == 30 {
                140.0
            } else {
                10.0 + (i % 5) as f64
            }
        });
        assert_eq!(run.misses.len(), 1);
        let cal = Calibration::from_run(&run).unwrap();
        assert_eq!(cal.offset_ms, 12.0);
        assert_eq!(cal.taps, 31);
        assert_eq!(cal.measured, run.ended);
        assert!(cal.sd_ms > 0.0);
        assert_eq!(
            Calibration::from_run(&calibration_run(15, |_| 5.0)),
            Err(15)
        );
        assert_eq!(
            Calibration::from_run(&calibration_run(16, |_| 5.0))
                .unwrap()
                .offset_ms,
            5.0
        );
    }

    #[test]
    fn calibration_file_round_trips() {
        let dir = std::env::temp_dir().join(format!("djembot-perf-{}-cal", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let path = dir.join("djembot").join("calibration.json");
        assert_eq!(Calibration::load(&path).unwrap(), None);
        let cal = Calibration {
            offset_ms: 12.0,
            sd_ms: 9.1,
            taps: 31,
            measured: "2026-10-11T10:12:03Z".into(),
        };
        cal.save(&path).unwrap();
        assert_eq!(Calibration::load(&path).unwrap(), Some(cal));
        fs::write(&path, "nonsense").unwrap();
        assert!(Calibration::load(&path).is_err());
        fs::remove_dir_all(&dir).unwrap();
    }
}
