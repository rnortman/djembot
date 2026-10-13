//! Playback state and the mixing function shared by the audio callback and
//! the tests.
//!
//! `mix` allocates nothing, frees nothing and takes no lock: buffers arrive
//! as `Arc`s inside the timeline, and the bank on the UI thread keeps every
//! buffer alive, so dropping a slot's `Arc` is only a refcount decrement.

use std::f32::consts::FRAC_PI_4;
use std::sync::Arc;

use crate::model::MAX_VOICES;
use crate::timeline::{Event, Timeline};

pub const SLOTS: usize = 64;

/// A sounding (or finished, while `pos == buf.len()`) one-shot.
#[derive(Clone, Default)]
struct Slot {
    buf: Option<Arc<[f32]>>,
    pos: usize,
    /// Equal-power pan folded into per-channel gains.
    gain_l: f32,
    gain_r: f32,
    /// Start order, for stealing the oldest slot.
    started: u64,
}

impl Slot {
    fn active(&self) -> bool {
        self.buf.as_ref().is_some_and(|b| self.pos < b.len())
    }
}

pub struct Mixer {
    pub timeline: Option<Arc<Timeline>>,
    /// The timeline that begins where the current one ends, if queued.
    pub next: Option<Arc<Timeline>>,
    /// The mutes `next` starts with.
    pub next_muted: [bool; MAX_VOICES],
    /// The frame at which the current timeline's frame 0 lies.
    pub origin: u64,
    /// Which link of the score is current: 0 for the loaded timeline, one
    /// more per swap to a queued one.
    pub segment: u32,
    pub playing: bool,
    /// Frames since the start of the first timeline's count-in, continuous
    /// across swaps.
    pub frame: u64,
    pub muted: [bool; MAX_VOICES],
    slots: [Slot; SLOTS],
    starts: u64,
}

impl Default for Mixer {
    fn default() -> Self {
        Mixer::new()
    }
}

impl Mixer {
    pub fn new() -> Mixer {
        Mixer {
            timeline: None,
            next: None,
            next_muted: [false; MAX_VOICES],
            origin: 0,
            segment: 0,
            playing: false,
            frame: 0,
            muted: [false; MAX_VOICES],
            slots: std::array::from_fn(|_| Slot::default()),
            starts: 0,
        }
    }

    /// Installs `tl` stopped at frame 0 with its mute flags, as segment 0
    /// with nothing queued; returns the timelines it displaced, the current
    /// one and the queued one. Sounding slots keep ringing.
    pub fn load(&mut self, tl: Arc<Timeline>) -> [Option<Arc<Timeline>>; 2] {
        self.muted = tl.mute;
        self.playing = false;
        self.frame = 0;
        self.origin = 0;
        self.segment = 0;
        [self.timeline.replace(tl), self.next.take()]
    }

    /// Queues `tl` to begin, with mutes `muted`, where the current timeline
    /// ends — if segment `after` is current; returns the timeline displaced:
    /// the previously queued one, or `tl` itself when `after` is stale.
    pub fn queue(
        &mut self,
        tl: Arc<Timeline>,
        after: u32,
        muted: [bool; MAX_VOICES],
    ) -> Option<Arc<Timeline>> {
        if after != self.segment || self.timeline.is_none() {
            return Some(tl);
        }
        self.next_muted = muted;
        self.next.replace(tl)
    }

    /// Starts playing; a finished timeline restarts from its own frame 0.
    pub fn play(&mut self) {
        if let Some(end) = self.timeline.as_ref().and_then(|t| t.end)
            && self.frame == self.origin + end
        {
            self.frame = 0;
            self.origin = 0;
        }
        self.playing = true;
    }

    pub fn pause(&mut self) {
        self.playing = false;
    }

    /// Stops at frame 0 of segment 0; returns the queued timeline it drops.
    pub fn stop(&mut self) -> Option<Arc<Timeline>> {
        self.playing = false;
        self.frame = 0;
        self.origin = 0;
        self.segment = 0;
        self.next.take()
    }

    /// Mutes or unmutes `voice` of segment `segment`: the current one, or
    /// the queued one. Anything else is over or not queued, and is ignored.
    pub fn set_mute(&mut self, voice: usize, mute: bool, segment: u32) {
        let muted = if segment == self.segment {
            &mut self.muted
        } else if segment == self.segment.wrapping_add(1) && self.next.is_some() {
            &mut self.next_muted
        } else {
            return;
        };
        if let Some(m) = muted.get_mut(voice) {
            *m = mute;
        }
    }

    /// Starts `buf` sounding at the next rendered frame, stealing the
    /// oldest slot when all are busy.
    pub(crate) fn strike(&mut self, buf: &Arc<[f32]>, gain: f32, pan: f32) {
        let idx = match self.slots.iter().position(|s| !s.active()) {
            Some(i) => i,
            None => {
                let mut oldest = 0;
                for (i, s) in self.slots.iter().enumerate() {
                    if s.started < self.slots[oldest].started {
                        oldest = i;
                    }
                }
                oldest
            }
        };
        let theta = (pan.clamp(-1.0, 1.0) + 1.0) * FRAC_PI_4;
        self.starts += 1;
        self.slots[idx] = Slot {
            buf: Some(buf.clone()),
            pos: 0,
            gain_l: gain * theta.cos(),
            gain_r: gain * theta.sin(),
            started: self.starts,
        };
    }

    /// Adds every active slot into stereo frames `out[from..to]`.
    fn render(&mut self, out: &mut [f32], from: usize, to: usize) {
        for s in &mut self.slots {
            let Some(buf) = &s.buf else { continue };
            let n = (to - from).min(buf.len() - s.pos);
            for (i, x) in buf[s.pos..s.pos + n].iter().enumerate() {
                let f = from + i;
                out[2 * f] += x * s.gain_l;
                out[2 * f + 1] += x * s.gain_r;
            }
            s.pos += n;
        }
    }
}

/// Fills `out` (interleaved stereo) with the next `out.len() / 2` frames.
/// At the end of a timeline the queued one, if any, takes over on the same
/// frame and the displaced timeline goes to `retire`; else the closing
/// events sound and playing stops.
pub fn mix(m: &mut Mixer, out: &mut [f32], retire: &mut impl FnMut(Arc<Timeline>)) {
    out.fill(0.0);
    let frames = out.len() / 2;
    let start = m.frame;
    let limit = start + frames as u64;
    let mut cursor = 0;
    let mut emit = |m: &mut Mixer, abs: u64, e: &Event| {
        let off = (abs - start) as usize;
        m.render(out, cursor, off);
        cursor = off;
        m.strike(&e.buf, e.gain, e.pan);
    };
    while m.playing {
        let Some(tl) = m.timeline.clone() else { break };
        let origin = m.origin;
        // The span to emit, relative to the timeline's frame 0.
        let a = m.frame - origin;
        let lim = limit - origin;
        let b = tl.end.map_or(lim, |e| e.min(lim));
        for e in &tl.count_in {
            if (a..b).contains(&e.at) {
                emit(m, origin + e.at, e);
            }
        }
        if tl.cycle_len > 0 && b > tl.count_in_len {
            let mut k = a.saturating_sub(tl.count_in_len) / tl.cycle_len;
            loop {
                if tl.cycles.is_some_and(|c| k >= c as u64) {
                    break;
                }
                let base = tl.count_in_len + k * tl.cycle_len;
                if base >= b {
                    break;
                }
                for e in &tl.events {
                    let at = base + e.at;
                    if at >= b {
                        break;
                    }
                    if at < a || m.muted.get(e.voice).copied().unwrap_or(false) {
                        continue;
                    }
                    emit(m, origin + at, e);
                }
                k += 1;
            }
        }
        let Some(end) = tl.end.filter(|&e| e <= lim) else {
            m.frame = limit;
            break;
        };
        let end = origin + end;
        m.frame = end;
        match m.next.take() {
            Some(next) => {
                if let Some(old) = m.timeline.replace(next) {
                    retire(old);
                }
                m.origin = end;
                m.segment = m.segment.wrapping_add(1);
                m.muted = m.next_muted;
            }
            None => {
                for e in &tl.closing {
                    if !m.muted.get(e.voice).copied().unwrap_or(false) {
                        emit(m, end + e.at, e);
                    }
                }
                m.playing = false;
            }
        }
    }
    m.render(out, cursor, frames);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Instrument;
    use crate::sounds::Bank;
    use crate::timeline::tests::exercise;
    use serde_json::{Value, json};

    const RATE: u32 = 48_000;
    const CYCLE: u64 = 128_000;

    fn impulse_bank() -> Bank {
        Bank::from_fn(|_| Arc::from(vec![1.0f32]))
    }

    fn ramp_bank() -> Bank {
        Bank::from_fn(|_| (0..1000).map(|i| (i + 1) as f32 / 1000.0).collect())
    }

    fn timeline(v: Value, bank: &mut Bank) -> Arc<Timeline> {
        Arc::new(Timeline::from(&exercise(v), bank, RATE).unwrap())
    }

    fn son_clave(cycles: u32) -> Value {
        json!({
            "tempo": 90, "beats": 4, "cycles": cycles,
            "voices": [{ "name": "clave", "instrument": "clave", "pulses": 16,
                         "onsets": [0, 3, 6, 10, 12] }]
        })
    }

    fn playing(tl: Arc<Timeline>) -> Mixer {
        let mut m = Mixer::new();
        m.load(tl);
        m.play();
        m
    }

    /// Mixes `frames` frames in chunks of `chunk`; returns interleaved output.
    fn run(m: &mut Mixer, frames: usize, chunk: usize) -> Vec<f32> {
        let mut out = vec![0.0; frames * 2];
        for c in out.chunks_mut(chunk * 2) {
            mix(m, c, &mut |_| {});
        }
        out
    }

    fn nonzero_frames(out: &[f32]) -> Vec<usize> {
        (0..out.len() / 2)
            .filter(|&f| out[2 * f] != 0.0 || out[2 * f + 1] != 0.0)
            .collect()
    }

    fn clave_positions(cycles: u64) -> Vec<usize> {
        (0..cycles)
            .flat_map(|k| [0u64, 3, 6, 10, 12].map(|p| (k * CYCLE + p * CYCLE / 16) as usize))
            .collect()
    }

    #[test]
    fn son_clave_two_cycles_impulses() {
        let mut bank = impulse_bank();
        let mut m = playing(timeline(son_clave(2), &mut bank));
        let out = run(&mut m, 2 * CYCLE as usize + 1000, 4096);
        let mut expected = clave_positions(2);
        expected.push(2 * CYCLE as usize);
        assert_eq!(nonzero_frames(&out), expected);
    }

    #[test]
    fn chunk_size_does_not_change_output() {
        let mut bank = impulse_bank();
        let tl = timeline(son_clave(2), &mut bank);
        let frames = 2 * CYCLE as usize + 500;
        let whole = run(&mut playing(tl.clone()), frames, frames);
        for chunk in [97, 256, 1024] {
            assert_eq!(
                run(&mut playing(tl.clone()), frames, chunk),
                whole,
                "{chunk}"
            );
        }
    }

    #[test]
    fn looping_wraps_forever() {
        let mut bank = impulse_bank();
        let mut v = son_clave(1);
        v.as_object_mut().unwrap().remove("cycles");
        let mut m = playing(timeline(v, &mut bank));
        let out = run(&mut m, 3 * CYCLE as usize, 1000);
        assert_eq!(nonzero_frames(&out), clave_positions(3));
        assert!(m.playing);
    }

    #[test]
    fn count_in_precedes_cycles() {
        let mut bank = impulse_bank();
        let mut v = son_clave(1);
        v["count_in"] = json!(2);
        let mut m = playing(timeline(v, &mut bank));
        let out = run(&mut m, 2 * CYCLE as usize, 333);
        let mut expected = vec![0, 32_000];
        expected.extend(clave_positions(1).iter().map(|p| p + 64_000));
        expected.push(64_000 + CYCLE as usize);
        assert_eq!(nonzero_frames(&out), expected);
        assert_eq!(m.frame, 64_000 + CYCLE);
    }

    #[test]
    fn muted_voice_is_silent() {
        let mut bank = impulse_bank();
        let tl = timeline(
            json!({
                "tempo": 90, "beats": 4, "cycles": 1,
                "voices": [
                    { "name": "clave", "instrument": "clave", "pulses": 16,
                      "onsets": [0, 3, 6, 10, 12] },
                    { "name": "click", "instrument": "click", "pulses": 4,
                      "onsets": [1, 2, 3] }
                ]
            }),
            &mut bank,
        );
        let mut m = playing(tl);
        m.set_mute(1, true, 0);
        let out = run(&mut m, CYCLE as usize, 1024);
        assert_eq!(nonzero_frames(&out), clave_positions(1));
    }

    #[test]
    fn mute_flag_from_timeline() {
        let mut bank = impulse_bank();
        let mut v = son_clave(1);
        v["voices"][0]["mute"] = json!(true);
        let mut m = playing(timeline(v, &mut bank));
        assert!(m.muted[0]);
        let out = run(&mut m, CYCLE as usize, 1024);
        assert!(nonzero_frames(&out).is_empty());
    }

    #[test]
    fn finite_exercise_stops_at_end() {
        let mut bank = impulse_bank();
        let tl = timeline(son_clave(2), &mut bank);
        let end = tl.end.unwrap();
        let mut m = playing(tl);
        run(&mut m, end as usize + 777, 1000);
        assert!(!m.playing);
        assert_eq!(m.frame, end);
        m.play();
        assert!(m.playing);
        assert_eq!(m.frame, 0);
    }

    #[test]
    fn last_stroke_rings_past_end() {
        let mut bank = ramp_bank();
        let tl = timeline(
            json!({
                "tempo": 90, "beats": 4, "cycles": 1, "ending": "none",
                "voices": [{ "name": "c", "instrument": "clave", "pulses": 16, "onsets": [15] }]
            }),
            &mut bank,
        );
        let mut m = playing(tl);
        let out = run(&mut m, CYCLE as usize + 2000, 512);
        let at = (15 * CYCLE / 16) as usize;
        // 8000 frames from onset to end: the ramp is complete regardless.
        assert_eq!(nonzero_frames(&out), (at..at + 1000).collect::<Vec<_>>());
        assert!(!m.playing);
    }

    #[test]
    fn slot_crosses_chunk_boundary_and_survives_load() {
        let mut bank = ramp_bank();
        let tl = timeline(
            json!({
                "tempo": 90, "beats": 4,
                "voices": [{ "name": "c", "instrument": "clave", "pulses": 16, "onsets": [1] }]
            }),
            &mut bank,
        );
        let mut m = playing(tl);
        let g = FRAC_PI_4.cos();
        // Onset at 8000; first chunk ends 10 frames later.
        let mut first = vec![0.0; 8010 * 2];
        mix(&mut m, &mut first, &mut |_| {});
        // Replace the timeline mid-ring and drop the old one.
        let other = timeline(son_clave(1), &mut bank);
        let [old, queued] = m.load(other);
        drop(old.unwrap());
        assert!(queued.is_none());
        assert!(!m.playing);
        let mut second = vec![0.0; 2000 * 2];
        mix(&mut m, &mut second, &mut |_| {});
        let left: Vec<f32> = first
            .iter()
            .chain(&second)
            .step_by(2)
            .copied()
            .skip(8000)
            .take(1001)
            .collect();
        for (i, x) in left[..1000].iter().enumerate() {
            assert_eq!(*x, (i + 1) as f32 / 1000.0 * g, "frame {i}");
        }
        assert_eq!(left[1000], 0.0);
    }

    #[test]
    fn equal_power_pan() {
        for (pan, l, r) in [
            (-1.0, 1.0, 0.0),
            (0.0, FRAC_PI_4.cos(), FRAC_PI_4.sin()),
            (1.0, 0.0, 1.0),
        ] {
            let mut bank = impulse_bank();
            let tl = timeline(
                json!({
                    "tempo": 90, "beats": 4, "cycles": 1,
                    "voices": [{ "name": "c", "instrument": "clave", "pulses": 4,
                                 "onsets": [0], "gain": 0.5, "pan": pan }]
                }),
                &mut bank,
            );
            let mut m = playing(tl);
            let out = run(&mut m, 16, 16);
            assert!((out[0] - 0.5 * l).abs() < 1e-6, "pan {pan}: L {}", out[0]);
            assert!((out[1] - 0.5 * r).abs() < 1e-6, "pan {pan}: R {}", out[1]);
        }
    }

    #[test]
    fn overflow_steals_oldest() {
        // Long constant buffers, distinguishable by level: the oldest clave
        // stroke (accented) 10, the other claves 1, the cowbell 1000. Hard
        // left pan puts each at exactly its level in the left channel.
        let mut bank = Bank::from_fn(|id| {
            let level = match (id.instrument, id.accent) {
                (Instrument::Cowbell, _) => 1000.0,
                (_, true) => 10.0,
                _ => 1.0,
            };
            Arc::from(vec![level; 100_000])
        });
        let mut claves: Vec<Value> = vec![json!({"at": 0, "accent": true})];
        claves.extend((1..SLOTS as u32).map(|p| json!(p)));
        let tl = timeline(
            json!({
                "tempo": 90, "beats": 4, "cycles": 1,
                "voices": [
                    { "name": "c", "instrument": "clave", "pulses": 128,
                      "onsets": claves, "pan": -1 },
                    { "name": "b", "instrument": "cowbell", "pulses": 2,
                      "onsets": [1], "pan": -1 }
                ]
            }),
            &mut bank,
        );
        // Clave strokes every 1000 frames fill all 64 slots by frame 63000;
        // the cowbell comes at 64000.
        let mut m = playing(tl);
        let out = run(&mut m, 70_000, 4096);
        let left = |f: usize| out[2 * f];
        assert_eq!(left(63_999), 10.0 + 63.0);
        // The oldest stroke is gone and the cowbell sounds.
        assert_eq!(left(64_000), 63.0 + 1000.0);
        assert_eq!(left(69_999), 63.0 + 1000.0);
    }

    #[test]
    fn stop_and_pause() {
        let mut bank = impulse_bank();
        let mut m = playing(timeline(son_clave(2), &mut bank));
        run(&mut m, 5000, 1000);
        m.pause();
        run(&mut m, 5000, 1000);
        assert_eq!(m.frame, 5000);
        m.stop();
        assert_eq!(m.frame, 0);
        assert!(!m.playing);
    }
    #[test]
    fn closing_events_sound_at_end_then_playing_stops() {
        let mut bank = impulse_bank();
        let tl = timeline(son_clave(1), &mut bank);
        let end = tl.end.unwrap() as usize;
        // A chunk ending exactly at `end` leaves the closing stroke to the
        // next call, which must sound it at its first frame.
        for chunk in [end, 1000, 4096] {
            let mut m = playing(tl.clone());
            let out = run(&mut m, end + 4096, chunk);
            let mut expected = clave_positions(1);
            expected.push(end);
            assert_eq!(nonzero_frames(&out), expected, "{chunk}");
            assert!(!m.playing);
            assert_eq!(m.frame, end as u64);
        }
    }

    #[test]
    fn closing_stroke_rings_past_end_and_obeys_the_mute() {
        let mut bank = ramp_bank();
        let mut v = son_clave(1);
        v["voices"][0]["onsets"] = json!([0]);
        let tl = timeline(v, &mut bank);
        let end = CYCLE as usize;
        let out = run(&mut playing(tl.clone()), end + 2000, 512);
        let mut expected: Vec<usize> = (0..1000).collect();
        expected.extend(end..end + 1000);
        assert_eq!(nonzero_frames(&out), expected);
        let mut m = playing(tl);
        m.set_mute(0, true, 0);
        assert!(nonzero_frames(&run(&mut m, end + 2000, 512)).is_empty());
    }

    #[test]
    fn closing_click_sounds_when_no_voice_has_pulse_zero() {
        let mut bank = impulse_bank();
        let mut v = son_clave(1);
        v["voices"][0]["onsets"] = json!([3]);
        v["voices"][0]["mute"] = json!(true);
        let out = run(
            &mut playing(timeline(v, &mut bank)),
            CYCLE as usize + 10,
            1000,
        );
        assert_eq!(nonzero_frames(&out), vec![CYCLE as usize]);
    }

    /// One cycle of son clave, then queued `second`: (mixer, end of the first).
    fn queued(second: Value, bank: &mut Bank) -> (Mixer, u64) {
        let mut first = son_clave(1);
        first["next"] = json!("attacca");
        let tl = timeline(first, bank);
        let end = tl.end.unwrap();
        let mut m = playing(tl);
        let second = timeline(second, bank);
        let muted = second.mute;
        assert!(m.queue(second, 0, muted).is_none());
        (m, end)
    }

    fn bell(cycles: u32) -> Value {
        json!({
            "tempo": 90, "beats": 4, "cycles": cycles, "count_in": 4,
            "voices": [
                { "name": "bell", "instrument": "bell", "pulses": 4, "onsets": [0, 2] },
                { "name": "click", "instrument": "click", "pulses": 4, "onsets": [1] }
            ]
        })
    }

    #[test]
    fn queued_timeline_starts_at_the_end_of_the_current_one_without_a_gap() {
        let mut bank = impulse_bank();
        for chunk in [CYCLE as usize, 1000, 4096, 300_000] {
            let (mut m, end) = queued(bell(1), &mut bank);
            let mut retired = Vec::new();
            let mut out = vec![0.0; (2 * CYCLE as usize + 1000) * 2];
            for c in out.chunks_mut(chunk * 2) {
                mix(&mut m, c, &mut |t| retired.push(t));
            }
            // The queued timeline begins at its own frame 0 on `end`: its
            // count-in clicks, then its cycle.
            let mut expected = clave_positions(1);
            let quarter = CYCLE as usize / 4;
            expected.extend((0..4).map(|b| end as usize + b * quarter));
            let c0 = end as usize + CYCLE as usize;
            expected.extend([c0, c0 + quarter, c0 + 2 * quarter]);
            expected.retain(|&f| f < out.len() / 2);
            assert_eq!(nonzero_frames(&out), expected, "{chunk}");
            assert_eq!(retired.len(), 1, "{chunk}");
            assert_eq!(m.segment, 1);
            assert_eq!(m.origin, end);
        }
    }

    #[test]
    fn frame_is_continuous_across_segments() {
        let mut bank = impulse_bank();
        let mut second = bell(1);
        second["count_in"] = json!(0);
        let (mut m, end) = queued(second, &mut bank);
        run(&mut m, end as usize - 100, 1000);
        assert_eq!((m.frame, m.origin, m.segment), (end - 100, 0, 0));
        run(&mut m, 300, 300);
        assert_eq!((m.frame, m.origin, m.segment), (end + 200, end, 1));
        assert!(m.next.is_none());
        // The second timeline ends with its closing downbeat and stops.
        let out = run(&mut m, CYCLE as usize, 1024);
        assert_eq!(m.frame, end + CYCLE);
        assert!(!m.playing);
        let last = nonzero_frames(&out).last().copied().unwrap();
        assert_eq!(last as u64, end + CYCLE - (end + 200));
    }

    #[test]
    fn swap_installs_the_queued_mutes() {
        let mut bank = impulse_bank();
        let mut first = son_clave(1);
        first["next"] = json!("attacca");
        let mut m = playing(timeline(first, &mut bank));
        let mut second = bell(1);
        second["count_in"] = json!(0);
        let tl = timeline(second, &mut bank);
        assert_eq!(tl.mute[..2], [false, false]);
        let mut muted = [false; MAX_VOICES];
        muted[0] = true;
        m.queue(tl, 0, muted);
        let out = run(&mut m, 2 * CYCLE as usize, 1000);
        assert_eq!(m.muted, muted);
        // Only the click voice of the second cycle sounds; no closing bell.
        let mut expected = clave_positions(1);
        expected.push((CYCLE + CYCLE / 4) as usize);
        assert_eq!(nonzero_frames(&out), expected);
    }

    #[test]
    fn set_mute_for_the_queued_segment_edits_its_mutes_before_and_after_the_swap() {
        let mut bank = impulse_bank();
        let (mut m, end) = queued(bell(1), &mut bank);
        m.set_mute(1, true, 1);
        assert_eq!(m.next_muted[..2], [false, true]);
        assert_eq!(
            m.muted[..2],
            [false, false],
            "the current link is untouched"
        );
        run(&mut m, end as usize + 10, 1000);
        assert_eq!(m.segment, 1);
        assert_eq!(m.muted[..2], [false, true]);
        m.set_mute(0, true, 1);
        assert_eq!(m.muted[..2], [true, true]);
    }

    #[test]
    fn set_mute_with_a_stale_segment_is_dropped() {
        let mut bank = impulse_bank();
        let (mut m, end) = queued(bell(1), &mut bank);
        run(&mut m, end as usize + 10, 1000);
        let before = (m.muted, m.next_muted);
        m.set_mute(0, true, 0);
        m.set_mute(0, true, 2);
        assert_eq!((m.muted, m.next_muted), before);
        // Segment 1 with nothing queued after it: segment 2 is not queued.
        let mut m = playing(timeline(son_clave(1), &mut bank));
        m.set_mute(0, true, 1);
        assert_eq!(m.next_muted, [false; MAX_VOICES]);
        assert_eq!(m.muted, [false; MAX_VOICES]);
    }

    #[test]
    fn queue_with_a_stale_after_is_dropped() {
        let mut bank = impulse_bank();
        let (mut m, end) = queued(bell(1), &mut bank);
        let late = timeline(bell(1), &mut bank);
        let back = m.queue(late.clone(), 1, [false; MAX_VOICES]);
        assert!(Arc::ptr_eq(&back.unwrap(), &late));
        run(&mut m, end as usize + 10, 1000);
        let back = m.queue(late.clone(), 0, [false; MAX_VOICES]);
        assert!(Arc::ptr_eq(&back.unwrap(), &late));
        assert!(m.next.is_none());
        // Nothing loaded: nothing to queue after.
        let mut m = Mixer::new();
        assert!(m.queue(late, 0, [false; MAX_VOICES]).is_some());
    }

    #[test]
    fn a_later_queue_replaces_the_queued_timeline() {
        let mut bank = impulse_bank();
        let (mut m, _) = queued(bell(1), &mut bank);
        let other = timeline(son_clave(2), &mut bank);
        let old = m.queue(other.clone(), 0, [false; MAX_VOICES]).unwrap();
        assert_eq!(old.cycles, Some(1));
        assert!(Arc::ptr_eq(m.next.as_ref().unwrap(), &other));
    }

    #[test]
    fn stop_clears_next_and_origin() {
        let mut bank = impulse_bank();
        let (mut m, end) = queued(bell(2), &mut bank);
        let mut third = bell(1);
        third["count_in"] = json!(0);
        run(&mut m, end as usize + 10, 1000);
        m.queue(timeline(third, &mut bank), 1, [false; MAX_VOICES]);
        let dropped = m.stop();
        assert!(dropped.is_some());
        assert!(m.next.is_none());
        assert_eq!((m.frame, m.origin, m.segment, m.playing), (0, 0, 0, false));
        // Load displaces both the current and the queued timeline.
        let (mut m, _) = queued(bell(1), &mut bank);
        let [current, next] = m.load(timeline(son_clave(1), &mut bank));
        assert!(current.is_some() && next.is_some());
        assert_eq!((m.origin, m.segment), (0, 0));
    }
}
