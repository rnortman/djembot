//! An exercise compiled to sample-stamped events, each carrying its buffer.

use std::sync::Arc;

use anyhow::Result;

use crate::model::{Ending, Exercise, Instrument, MAX_VOICES, Next};
use crate::sounds::{Bank, SoundId};

/// One sound start. `at` is relative to the start of the cycle (cycle
/// events) or of the count-in (count-in events).
#[derive(Clone, Debug)]
pub struct Event {
    pub at: u64,
    pub buf: Arc<[f32]>,
    pub gain: f32,
    pub pan: f32,
    /// Voice index; `MAX_VOICES` for count-in clicks, which no mute affects.
    pub voice: usize,
    /// Pulse index within the voice's cycle; beat index for count-in clicks.
    pub pulse: u32,
}

#[derive(Clone, Debug)]
pub struct Timeline {
    pub cycle_len: u64,
    /// One cycle's events, sorted by `at`.
    pub events: Vec<Event>,
    /// Clicks at beat boundaries before cycle 0, sorted by `at`.
    pub count_in: Vec<Event>,
    pub count_in_len: u64,
    pub cycles: Option<u32>,
    pub mute: [bool; MAX_VOICES],
    /// `count_in_len + cycles * cycle_len` for a finite exercise.
    pub end: Option<u64>,
    /// Events sounded at `end` when the timeline finishes, `at == 0`: the
    /// closing downbeat of a finite exercise, empty otherwise.
    pub closing: Vec<Event>,
}

/// `round(num / den)` for positive integers.
fn div_round(num: u64, den: u64) -> u64 {
    (2 * num + den) / (2 * den)
}

impl Timeline {
    /// Compiles `ex` at `sample_rate`. Every buffer the result refers to is
    /// fetched from `bank` here; fails if the bank cannot render one.
    pub fn from(ex: &Exercise, bank: &mut Bank, sample_rate: u32) -> Result<Timeline> {
        let beat_len = 60.0 / ex.tempo * sample_rate as f64;
        let cycle_len = (ex.beats as f64 * beat_len).round() as u64;
        let mut events = Vec::new();
        for (vi, voice) in ex.voices.iter().enumerate() {
            for onset in &voice.onsets {
                let buf = bank.get(SoundId {
                    instrument: voice.instrument,
                    tone: onset.tone,
                    accent: onset.accent,
                })?;
                events.push(Event {
                    at: div_round(onset.at as u64 * cycle_len, voice.pulses as u64),
                    buf,
                    gain: voice.gain,
                    pan: voice.pan,
                    voice: vi,
                    pulse: onset.at,
                });
            }
        }
        events.sort_by_key(|e| e.at);
        let click = bank.get(SoundId {
            instrument: Instrument::Click,
            tone: 'x',
            accent: false,
        })?;
        let count_in = (0..ex.count_in)
            .map(|b| Event {
                at: (b as f64 * beat_len).round() as u64,
                buf: click.clone(),
                gain: 1.0,
                pan: 0.0,
                voice: MAX_VOICES,
                pulse: b,
            })
            .collect();
        let count_in_len = (ex.count_in as f64 * beat_len).round() as u64;
        // Only the end of a score closes: a block followed attacca hands over
        // to the next one instead.
        let closes = ex.cycles.is_some() && ex.next == Next::Stop && ex.ending == Ending::Downbeat;
        let closing = if closes {
            let first: Vec<Event> = events.iter().filter(|e| e.pulse == 0).cloned().collect();
            if first.is_empty() {
                let accent = bank.get(SoundId {
                    instrument: Instrument::Click,
                    tone: 'x',
                    accent: true,
                })?;
                vec![Event {
                    at: 0,
                    buf: accent,
                    gain: 1.0,
                    pan: 0.0,
                    voice: MAX_VOICES,
                    pulse: 0,
                }]
            } else {
                first
            }
        } else {
            Vec::new()
        };
        Ok(Timeline {
            cycle_len,
            events,
            count_in,
            count_in_len,
            cycles: ex.cycles,
            mute: ex.initial_mutes(),
            end: ex.cycles.map(|c| count_in_len + c as u64 * cycle_len),
            closing,
        })
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use crate::model::Diagnostics;
    use serde_json::{Value, json};
    use std::sync::Mutex;

    pub fn exercise(v: Value) -> Exercise {
        let mut diag = Diagnostics::default();
        let ex = Exercise::from_value(&v, 1, "", &mut diag);
        assert!(diag.errors.is_empty(), "{:?}", diag.errors);
        ex.unwrap()
    }

    /// A bank that hands out a distinct one-sample buffer per `SoundId`
    /// and records every request.
    fn recording_bank() -> (Bank, Arc<Mutex<Vec<SoundId>>>) {
        let log = Arc::new(Mutex::new(Vec::new()));
        let l = log.clone();
        let bank = Bank::from_fn(move |id| {
            let mut l = l.lock().unwrap();
            l.push(id);
            Arc::from(vec![l.len() as f32])
        });
        (bank, log)
    }

    fn twelve_sixteen() -> Exercise {
        exercise(json!({
            "tempo": 90, "beats": 4, "cycles": 8, "count_in": 4,
            "voices": [
                { "name": "bell", "instrument": "bell", "pulses": 12,
                  "onsets": [0, 2, 4, 5, 7, 9, 11], "gain": 0.7, "pan": -0.4 },
                { "name": "clave", "instrument": "clave", "pulses": 16,
                  "onsets": [2, 4, 8, 11, 14], "mute": true },
                { "name": "conga", "instrument": "conga", "pulses": 4,
                  "onsets": [{"at": 0, "tone": "s", "accent": true}, 2] }
            ]
        }))
    }

    #[test]
    fn cycle_len_90bpm_4_beats_48k() {
        let (mut bank, _) = recording_bank();
        let t = Timeline::from(&twelve_sixteen(), &mut bank, 48_000).unwrap();
        assert_eq!(t.cycle_len, 128_000);
    }

    #[test]
    fn events_carry_the_bank_buffer_for_their_sound() {
        let (mut bank, log) = recording_bank();
        let ex = twelve_sixteen();
        let t = Timeline::from(&ex, &mut bank, 48_000).unwrap();
        for e in &t.events {
            let voice = &ex.voices[e.voice];
            let onset = voice.onsets.iter().find(|o| o.at == e.pulse).unwrap();
            let id = SoundId {
                instrument: voice.instrument,
                tone: onset.tone,
                accent: onset.accent,
            };
            assert!(Arc::ptr_eq(&e.buf, &bank.get(id).unwrap()), "{id:?}");
        }
        let click = bank
            .get(SoundId {
                instrument: Instrument::Click,
                tone: 'x',
                accent: false,
            })
            .unwrap();
        assert_eq!(t.count_in.len(), 4);
        for e in &t.count_in {
            assert!(Arc::ptr_eq(&e.buf, &click));
        }
        let log = log.lock().unwrap();
        assert!(log.contains(&SoundId {
            instrument: Instrument::Conga,
            tone: 's',
            accent: true,
        }));
        assert!(log.contains(&SoundId {
            instrument: Instrument::Conga,
            tone: 'o',
            accent: false,
        }));
    }

    #[test]
    fn at_values_across_subdivisions() {
        let (mut bank, _) = recording_bank();
        let t = Timeline::from(&twelve_sixteen(), &mut bank, 48_000).unwrap();
        let at = |voice: usize, pulse: u32| {
            t.events
                .iter()
                .find(|e| e.voice == voice && e.pulse == pulse)
                .unwrap()
                .at
        };
        assert_eq!(at(1, 8), 64_000);
        assert_eq!(at(0, 4), 42_667);
        assert_eq!(at(1, 11), 88_000);
        assert_eq!(at(2, 2), 64_000);
        assert_eq!(at(0, 0), 0);
    }

    #[test]
    fn half_cycle_lines_up_for_12_and_16() {
        let (mut bank, _) = recording_bank();
        let ex = exercise(json!({
            "tempo": 90, "beats": 4,
            "voices": [
                { "name": "a", "instrument": "bell", "pulses": 12, "onsets": [6] },
                { "name": "b", "instrument": "clave", "pulses": 16, "onsets": [8] }
            ]
        }));
        let t = Timeline::from(&ex, &mut bank, 48_000).unwrap();
        assert_eq!(t.events[0].at, 64_000);
        assert_eq!(t.events[1].at, 64_000);
    }

    #[test]
    fn count_in_precedes_cycle_zero() {
        let (mut bank, _) = recording_bank();
        let t = Timeline::from(&twelve_sixteen(), &mut bank, 48_000).unwrap();
        assert_eq!(t.count_in_len, 128_000);
        let ats: Vec<u64> = t.count_in.iter().map(|e| e.at).collect();
        assert_eq!(ats, vec![0, 32_000, 64_000, 96_000]);
        assert!(t.count_in.iter().all(|e| e.at < t.count_in_len));
        assert!(t.count_in.iter().all(|e| e.voice == MAX_VOICES));
    }

    #[test]
    fn events_sorted_and_end() {
        let (mut bank, _) = recording_bank();
        let t = Timeline::from(&twelve_sixteen(), &mut bank, 48_000).unwrap();
        assert!(t.events.windows(2).all(|w| w[0].at <= w[1].at));
        assert_eq!(t.events.len(), 7 + 5 + 2);
        assert_eq!(t.cycles, Some(8));
        assert_eq!(t.end, Some(128_000 + 8 * 128_000));
        assert_eq!(t.mute[..3], [false, true, false]);
    }

    #[test]
    fn looping_has_no_end() {
        let (mut bank, _) = recording_bank();
        let ex = exercise(json!({
            "tempo": 120, "beats": 3,
            "voices": [{ "name": "a", "instrument": "click", "pulses": 3, "onsets": [0] }]
        }));
        let t = Timeline::from(&ex, &mut bank, 44_100).unwrap();
        assert_eq!(t.cycle_len, 66_150);
        assert_eq!(t.count_in_len, 0);
        assert!(t.count_in.is_empty());
        assert_eq!(t.end, None);
    }
    #[test]
    fn closing_events_are_the_pulse_zero_onsets_at_end() {
        let (mut bank, _) = recording_bank();
        let ex = twelve_sixteen();
        let t = Timeline::from(&ex, &mut bank, 48_000).unwrap();
        let mut got: Vec<(usize, u32)> = t.closing.iter().map(|e| (e.voice, e.pulse)).collect();
        got.sort();
        assert_eq!(got, vec![(0, 0), (2, 0)]);
        for e in &t.closing {
            assert_eq!(e.at, 0);
            let same = t
                .events
                .iter()
                .find(|c| c.voice == e.voice && c.pulse == 0)
                .unwrap();
            assert!(Arc::ptr_eq(&e.buf, &same.buf));
            assert_eq!((e.gain, e.pan), (same.gain, same.pan));
        }
    }

    #[test]
    fn closing_is_an_accented_click_when_no_voice_has_pulse_zero() {
        let (mut bank, _) = recording_bank();
        let ex = exercise(json!({
            "tempo": 90, "beats": 4, "cycles": 2,
            "voices": [{ "name": "c", "instrument": "clave", "pulses": 16,
                         "onsets": [2, 4, 8, 11, 14] }]
        }));
        let t = Timeline::from(&ex, &mut bank, 48_000).unwrap();
        assert_eq!(t.closing.len(), 1);
        let e = &t.closing[0];
        assert_eq!((e.at, e.voice, e.pulse), (0, MAX_VOICES, 0));
        let accent = bank
            .get(SoundId {
                instrument: Instrument::Click,
                tone: 'x',
                accent: true,
            })
            .unwrap();
        assert!(Arc::ptr_eq(&e.buf, &accent));
    }

    #[test]
    fn closing_is_empty_for_ending_none_and_for_looping() {
        let (mut bank, _) = recording_bank();
        let mut v = json!({
            "tempo": 90, "beats": 4, "cycles": 2, "ending": "none",
            "voices": [{ "name": "a", "instrument": "click", "pulses": 4, "onsets": [0] }]
        });
        let t = Timeline::from(&exercise(v.clone()), &mut bank, 48_000).unwrap();
        assert!(t.closing.is_empty());
        v.as_object_mut().unwrap().remove("cycles");
        v.as_object_mut().unwrap().remove("ending");
        let t = Timeline::from(&exercise(v), &mut bank, 48_000).unwrap();
        assert!(t.closing.is_empty());
    }

    #[test]
    fn closing_is_empty_when_a_block_follows_attacca() {
        let (mut bank, _) = recording_bank();
        let v = json!({
            "tempo": 90, "beats": 4, "cycles": 2, "next": "attacca",
            "voices": [{ "name": "a", "instrument": "click", "pulses": 4, "onsets": [0] }]
        });
        let t = Timeline::from(&exercise(v), &mut bank, 48_000).unwrap();
        assert!(t.closing.is_empty());
        assert_eq!(t.end, Some(2 * 128_000));
    }
}
