//! Exercise format: parsing from JSON and validation.
//!
//! Parsing walks a `serde_json::Value` by hand so that every problem in an
//! exercise is reported, not just the first one serde would stop at. The
//! serde form of an `Exercise` is that same authoring JSON, deserialized
//! through the same validation.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::input::keys;

pub const MAX_VOICES: usize = 9;
pub const TEMPO_MIN: f64 = 20.0;
pub const TEMPO_MAX: f64 = 300.0;
pub const GM_MIN: u8 = 35;
pub const GM_MAX: u8 = 81;
/// Upper bounds on `beats`, `count_in` and `pulses`. With tempo at least
/// `TEMPO_MIN`, they keep every sample position a timeline computes far
/// inside `u64` for any `cycles`, and the count-in to a few dozen events.
pub const MAX_BEATS: u32 = 64;
pub const MAX_COUNT_IN: u32 = 64;
pub const MAX_PULSES: u32 = 1024;
/// Bounds and default of `window_ms`, the half-width of the timing window
/// within which a stroke matches a target.
pub const WINDOW_MS_MIN: f64 = 10.0;
pub const WINDOW_MS_MAX: f64 = 500.0;
pub const DEFAULT_WINDOW_MS: f64 = 100.0;

/// A percussion instrument: a named member of the GM kit subset, or a raw
/// GM percussion key.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Instrument {
    Click,
    Clave,
    Bell,
    Cowbell,
    Shaker,
    Block,
    Conga,
    Bongo,
    Timbale,
    Guiro,
    Gm(u8),
}

/// (instrument, name, [(tone letter, GM key)]), default tone first.
type Entry = (Instrument, &'static str, &'static [(char, u8)]);

/// Named instruments with their tones and GM keys.
const TABLE: &[Entry] = &[
    (Instrument::Click, "click", &[('x', 37)]),
    (Instrument::Clave, "clave", &[('x', 75)]),
    (Instrument::Bell, "bell", &[('h', 67), ('l', 68)]),
    (Instrument::Cowbell, "cowbell", &[('x', 56)]),
    (Instrument::Shaker, "shaker", &[('x', 70)]),
    (Instrument::Block, "block", &[('h', 76), ('l', 77)]),
    (
        Instrument::Conga,
        "conga",
        &[('o', 63), ('s', 62), ('b', 64)],
    ),
    (Instrument::Bongo, "bongo", &[('h', 60), ('l', 61)]),
    (Instrument::Timbale, "timbale", &[('h', 65), ('l', 66)]),
    (Instrument::Guiro, "guiro", &[('s', 73), ('l', 74)]),
];

impl Instrument {
    /// Every named instrument (excludes `gm:NN`).
    pub fn named() -> impl Iterator<Item = Instrument> {
        TABLE.iter().map(|(i, _, _)| *i)
    }

    pub fn parse(s: &str) -> Result<Instrument, String> {
        if let Some(n) = s.strip_prefix("gm:") {
            let key: u8 = n
                .parse()
                .map_err(|_| format!("unknown instrument \"{s}\" (gm:NN takes a number)"))?;
            if !(GM_MIN..=GM_MAX).contains(&key) {
                return Err(format!(
                    "instrument \"{s}\": GM percussion key must be in {GM_MIN}..={GM_MAX}"
                ));
            }
            return Ok(Instrument::Gm(key));
        }
        TABLE
            .iter()
            .find(|(_, name, _)| *name == s)
            .map(|(i, _, _)| *i)
            .ok_or_else(|| {
                let names: Vec<&str> = TABLE.iter().map(|(_, n, _)| *n).collect();
                format!(
                    "unknown instrument \"{s}\" (known: {}, gm:NN)",
                    names.join(", ")
                )
            })
    }

    pub fn name(self) -> String {
        match self {
            Instrument::Gm(k) => format!("gm:{k}"),
            _ => self.entry().1.to_string(),
        }
    }

    fn entry(self) -> &'static Entry {
        TABLE
            .iter()
            .find(|(i, _, _)| *i == self)
            .expect("every named instrument is in TABLE")
    }

    /// Tone letters, default first.
    pub fn tones(self) -> Vec<char> {
        match self {
            Instrument::Gm(_) => vec!['x'],
            _ => self.entry().2.iter().map(|(t, _)| *t).collect(),
        }
    }

    pub fn default_tone(self) -> char {
        self.tones()[0]
    }

    /// The tone `s` names: exactly one of this instrument's tone letters.
    pub fn parse_tone(self, s: &str) -> Result<char, String> {
        let tones = self.tones();
        let mut chars = s.chars();
        match (chars.next(), chars.next()) {
            (Some(c), None) if tones.contains(&c) => Ok(c),
            _ => Err(format!(
                "tone \"{s}\" is not a tone of {} (tones: {})",
                self.name(),
                tones.into_iter().collect::<String>()
            )),
        }
    }

    /// GM percussion key for a tone of this instrument.
    pub fn gm_key(self, tone: char) -> Option<u8> {
        match self {
            Instrument::Gm(k) => (tone == 'x').then_some(k),
            _ => self
                .entry()
                .2
                .iter()
                .find(|(t, _)| *t == tone)
                .map(|(_, k)| *k),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Onset {
    pub at: u32,
    /// Resolved tone letter (the instrument's default when not given).
    pub tone: char,
    pub accent: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Voice {
    pub name: String,
    pub instrument: Instrument,
    pub pulses: u32,
    pub onsets: Vec<Onset>,
    pub gain: f32,
    pub pan: f32,
    pub mute: bool,
    /// The keys the user plays this voice on, in the instrument's tone
    /// order; empty when the computer plays it.
    pub keys: Vec<KeyBinding>,
    /// Whether the user's strokes on this part are judged without sounding;
    /// only ever true for a voice with keys.
    pub silent: bool,
}

/// One tone of a player voice, bound to a key name from `input::keys`.
#[derive(Clone, Debug, PartialEq)]
pub struct KeyBinding {
    pub tone: char,
    pub key: String,
}

/// Serializes to authoring JSON; deserializes through `from_value`, so a
/// deserialized exercise satisfies every validation rule.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(into = "Value", try_from = "Value")]
pub struct Exercise {
    /// Resolved label: `label`, else `title`, else `block N`.
    pub label: String,
    pub title: Option<String>,
    /// Beats per minute, already clamped to `TEMPO_MIN..=TEMPO_MAX`.
    pub tempo: f64,
    pub beats: u32,
    pub cycles: Option<u32>,
    /// How the window shows where the block is in its cycles.
    pub cycle_count: CycleCount,
    pub count_in: u32,
    /// Whether the window starts with the grid and necklace hidden.
    pub hidden: bool,
    /// Half-width of the timing window in ms, as written in the exercise.
    pub window_ms: Option<f64>,
    pub voices: Vec<Voice>,
    /// How a finite block ends.
    pub ending: Ending,
    /// What follows the block's last cycle.
    // TODO(advance-on-threshold): a rule that repeats the block until the
    // user passes.
    pub next: Next,
    /// Whether the window draws where the music is: the playhead and the
    /// flash of the strokes it passes.
    pub playhead: bool,
    /// When the window shows the user's judgments. `After`: shown once
    /// the performance has retired, whatever retired it.
    pub feedback: Feedback,
}

/// When the window shows the user's judgments: the marks on the grid and
/// the necklace, and the tally in the block header.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Feedback {
    /// As each stroke is judged.
    #[default]
    Live,
    /// Once the performance is over.
    After,
    /// Never.
    None,
}

impl Feedback {
    pub fn name(self) -> &'static str {
        match self {
            Feedback::Live => "live",
            Feedback::After => "after",
            Feedback::None => "none",
        }
    }
}

/// What follows a finite exercise's last cycle.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Next {
    /// Nothing: the block ends (see `Ending`).
    #[default]
    Stop,
    /// The next block in the lesson begins on the same clock, without a
    /// gap and without its count-in.
    Attacca,
}

impl Next {
    pub fn name(self) -> &'static str {
        match self {
            Next::Stop => "stop",
            Next::Attacca => "attacca",
        }
    }
}

/// How the window shows where a block is in its cycles: the status line's
/// cycle text and, under `Countdown`, the header's last-cycle cue.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CycleCount {
    /// Cycles left after this one, then `last cycle`. Needs `cycles`.
    Countdown,
    /// The 1-based cycle, out of `cycles` or `∞`.
    #[default]
    Count,
    /// Nothing.
    None,
}

impl CycleCount {
    pub fn name(self) -> &'static str {
        match self {
            CycleCount::Countdown => "countdown",
            CycleCount::Count => "count",
            CycleCount::None => "none",
        }
    }
}

/// What sounds when a finite exercise's last cycle is over.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Ending {
    /// One closing stroke on the next "one", which a part also gets as a
    /// target.
    #[default]
    Downbeat,
    /// Nothing: the music stops after the last cycle.
    None,
}

impl Ending {
    pub fn name(self) -> &'static str {
        match self {
            Ending::Downbeat => "downbeat",
            Ending::None => "none",
        }
    }
}

/// Problems found while parsing; errors make the input invalid, warnings
/// do not.
#[derive(Debug, Default)]
pub struct Diagnostics {
    pub errors: Vec<String>,
    pub warnings: Vec<String>,
}

const EXERCISE_KEYS: &[&str] = &[
    "label",
    "title",
    "tempo",
    "beats",
    "cycles",
    "count_in",
    "hidden",
    "window_ms",
    "voices",
    "source",
    "ending",
    "next",
    "playhead",
    "feedback",
    "cycle_count",
];
const VOICE_KEYS: &[&str] = &[
    "name",
    "instrument",
    "pulses",
    "onsets",
    "gain",
    "pan",
    "mute",
    "keys",
    "silent",
];
const ONSET_KEYS: &[&str] = &["at", "tone", "accent"];

/// The label an exercise object would get as block `index` (1-based),
/// without validating anything else.
pub fn label_of(v: &Value, index: usize) -> String {
    let get = |k: &str| v.get(k).and_then(Value::as_str);
    resolve_label(get("label"), get("title"), index)
}

/// `label`, else `title`, else `block {index}`.
fn resolve_label(label: Option<&str>, title: Option<&str>, index: usize) -> String {
    label
        .or(title)
        .map_or_else(|| format!("block {index}"), str::to_string)
}

impl Exercise {
    /// Which voices start muted, by voice index.
    pub fn initial_mutes(&self) -> [bool; MAX_VOICES] {
        let mut mute = [false; MAX_VOICES];
        for (m, v) in mute.iter_mut().zip(&self.voices) {
            *m = v.mute;
        }
        mute
    }

    /// The timing window in ms: `window_ms`, else `DEFAULT_WINDOW_MS`.
    pub fn window_ms(&self) -> f64 {
        self.window_ms.unwrap_or(DEFAULT_WINDOW_MS)
    }

    /// Whether the exercise gives the user a part: a voice with `keys`.
    pub fn has_part(&self) -> bool {
        self.player_voices().next().is_some()
    }

    /// Indices of the voices the user plays (those with `keys`).
    pub fn player_voices(&self) -> impl Iterator<Item = usize> + '_ {
        self.voices
            .iter()
            .enumerate()
            .filter(|(_, v)| !v.keys.is_empty())
            .map(|(i, _)| i)
    }

    /// Parses and validates an exercise object. `index` is the 1-based
    /// block number used for the default label. Every problem is pushed to
    /// `diag` prefixed with `ctx`; `None` is returned if any error was found.
    pub fn from_value(
        v: &Value,
        index: usize,
        ctx: &str,
        diag: &mut Diagnostics,
    ) -> Option<Exercise> {
        let errors_before = diag.errors.len();
        let mut err = |msg: String| diag.errors.push(format!("{ctx}{msg}"));
        let Some(obj) = v.as_object() else {
            err("exercise must be a JSON object".into());
            return None;
        };
        unknown_keys(obj, EXERCISE_KEYS, &mut err);

        let label = opt_str(obj, "label", &mut err);
        let title = opt_str(obj, "title", &mut err);
        let tempo = match obj.get("tempo") {
            None => {
                err("tempo: missing".into());
                None
            }
            Some(t) => match t.as_f64() {
                Some(t) => Some(t),
                None => {
                    err("tempo: must be a number".into());
                    None
                }
            },
        };
        let beats = req_uint(obj, "beats", 1, MAX_BEATS, &mut err);
        let cycles = opt_uint(obj, "cycles", 1, u32::MAX, &mut err);
        let count_in = opt_uint(obj, "count_in", 0, MAX_COUNT_IN, &mut err).unwrap_or(0);
        let hidden = opt_bool(obj, "hidden", false, &mut err).unwrap_or(false);
        let playhead = opt_bool(obj, "playhead", true, &mut err).unwrap_or(true);
        let window_ms = match obj.get("window_ms") {
            None => None,
            Some(w) => match w.as_f64() {
                Some(w) if (WINDOW_MS_MIN..=WINDOW_MS_MAX).contains(&w) => Some(w),
                _ => {
                    err(format!(
                        "window_ms: must be a number from {WINDOW_MS_MIN} to {WINDOW_MS_MAX}, got {w}"
                    ));
                    None
                }
            },
        };
        if let Some(s) = obj.get("source")
            && !s.is_string()
        {
            err("source: must be a string".into());
        }
        let ending = opt_word(
            obj,
            "ending",
            &[Ending::Downbeat, Ending::None],
            Ending::name,
            &mut err,
        );
        let next = opt_word(
            obj,
            "next",
            &[Next::Stop, Next::Attacca],
            Next::name,
            &mut err,
        );
        let feedback = opt_word(
            obj,
            "feedback",
            &[Feedback::Live, Feedback::After, Feedback::None],
            Feedback::name,
            &mut err,
        );
        let cycle_count = opt_word(
            obj,
            "cycle_count",
            &[CycleCount::Countdown, CycleCount::Count, CycleCount::None],
            CycleCount::name,
            &mut err,
        );

        let mut voices = Vec::new();
        match obj.get("voices") {
            None => err("voices: missing".into()),
            Some(Value::Array(list)) => {
                if list.is_empty() {
                    err("voices: at least one voice is required".into());
                }
                if list.len() > MAX_VOICES {
                    err(format!(
                        "voices: {} voices, at most {MAX_VOICES} are allowed",
                        list.len()
                    ));
                }
                // Key name → the 1-based voice that binds it.
                let mut bound = HashMap::new();
                for (i, vv) in list.iter().enumerate() {
                    if let Some(voice) = parse_voice(vv, i + 1, &mut bound, &mut err) {
                        voices.push(voice);
                    }
                }
            }
            Some(_) => err("voices: must be an array".into()),
        }

        let tempo = tempo.map(|t| {
            let clamped = t.clamp(TEMPO_MIN, TEMPO_MAX);
            if clamped != t {
                diag.warnings.push(format!(
                    "{ctx}tempo: {t} is outside {TEMPO_MIN}..{TEMPO_MAX}, clamped to {clamped}"
                ));
            }
            clamped
        });

        if diag.errors.len() != errors_before {
            return None;
        }
        // Every field parsed, so a `None` here means the key is absent.
        if cycle_count == CycleCount::Countdown && cycles.is_none() {
            diag.errors.push(format!(
                "{ctx}cycle_count: \"countdown\" needs cycles (a looping block never ends)"
            ));
            return None;
        }
        Some(Exercise {
            label: resolve_label(label.as_deref(), title.as_deref(), index),
            title,
            tempo: tempo?,
            beats: beats?,
            cycles,
            cycle_count,
            count_in,
            hidden,
            window_ms,
            voices,
            ending,
            next,
            playhead,
            feedback,
        })
    }
}

impl From<Exercise> for Value {
    fn from(e: Exercise) -> Value {
        let voices: Vec<Value> = e
            .voices
            .iter()
            .map(|v| {
                let onsets: Vec<Value> = v
                    .onsets
                    .iter()
                    .map(|o| json!({ "at": o.at, "tone": o.tone.to_string(), "accent": o.accent }))
                    .collect();
                let mut voice = json!({
                    "name": v.name,
                    "instrument": v.instrument.name(),
                    "pulses": v.pulses,
                    "onsets": onsets,
                    "gain": v.gain,
                    "pan": v.pan,
                    "mute": v.mute,
                });
                // Emitted only when set, so a window that predates the key
                // still reads every lesson that does not use it.
                if v.silent {
                    voice["silent"] = true.into();
                }
                if !v.keys.is_empty() {
                    let keys: Map<String, Value> = v
                        .keys
                        .iter()
                        .map(|b| (b.tone.to_string(), Value::from(b.key.clone())))
                        .collect();
                    voice["keys"] = keys.into();
                }
                voice
            })
            .collect();
        let mut obj = json!({
            "label": e.label,
            "tempo": e.tempo,
            "beats": e.beats,
            "count_in": e.count_in,
            "hidden": e.hidden,
            "voices": voices,
        });
        if let Some(t) = e.title {
            obj["title"] = t.into();
        }
        if let Some(c) = e.cycles {
            obj["cycles"] = c.into();
        }
        if let Some(w) = e.window_ms {
            obj["window_ms"] = w.into();
        }
        // Emitted only when not the default, so a window that predates the
        // key still reads every lesson that does not use it.
        put_word(&mut obj, "ending", e.ending, Ending::name);
        put_word(&mut obj, "next", e.next, Next::name);
        if !e.playhead {
            obj["playhead"] = false.into();
        }
        put_word(&mut obj, "feedback", e.feedback, Feedback::name);
        put_word(&mut obj, "cycle_count", e.cycle_count, CycleCount::name);
        obj
    }
}

impl TryFrom<Value> for Exercise {
    type Error = String;

    fn try_from(v: Value) -> Result<Exercise, String> {
        let mut diag = Diagnostics::default();
        Exercise::from_value(&v, 1, "", &mut diag).ok_or_else(|| diag.errors.join("; "))
    }
}

/// Parses voice `n` (1-based). `bound` maps each key name already bound by
/// an earlier voice to that voice's number; this voice's keys are added.
fn parse_voice(
    v: &Value,
    n: usize,
    bound: &mut HashMap<String, usize>,
    err: &mut impl FnMut(String),
) -> Option<Voice> {
    let Some(obj) = v.as_object() else {
        err(format!("voice {n}: must be a JSON object"));
        return None;
    };
    let mut verr = |msg: String| err(format!("voice {n}: {msg}"));
    unknown_keys(obj, VOICE_KEYS, &mut verr);

    let name = match obj.get("name") {
        None => {
            verr("name: missing".into());
            None
        }
        Some(Value::String(s)) => Some(s.clone()),
        Some(_) => {
            verr("name: must be a string".into());
            None
        }
    };
    let instrument = match obj.get("instrument") {
        None => {
            verr("instrument: missing".into());
            None
        }
        Some(Value::String(s)) => match Instrument::parse(s) {
            Ok(i) => Some(i),
            Err(e) => {
                verr(format!("instrument: {e}"));
                None
            }
        },
        Some(_) => {
            verr("instrument: must be a string".into());
            None
        }
    };
    let pulses = req_uint(obj, "pulses", 1, MAX_PULSES, &mut verr);
    let gain = opt_range(obj, "gain", 0.0, 1.0, 1.0, &mut verr);
    let pan = opt_range(obj, "pan", -1.0, 1.0, 0.0, &mut verr);
    let keys = match obj.get("keys") {
        None => Some(Vec::new()),
        Some(k) => parse_keys(k, instrument, n, bound, &mut verr),
    };
    // A player voice's computer rendering is a guide track, off by default.
    let player = keys.as_ref().is_some_and(|k| !k.is_empty());
    let mute = opt_bool(obj, "mute", player, &mut verr);
    let silent = match opt_bool(obj, "silent", false, &mut verr) {
        Some(true) if keys.as_ref().is_some_and(Vec::is_empty) => {
            verr("silent: only a voice with keys has strokes to silence".into());
            None
        }
        s => s,
    };

    let mut onsets = Vec::new();
    match obj.get("onsets") {
        None => verr("onsets: missing".into()),
        Some(Value::Array(list)) => {
            for (j, ov) in list.iter().enumerate() {
                if let Some(o) = parse_onset(ov, j, instrument, pulses, &mut verr) {
                    if onsets.iter().any(|p: &Onset| p.at == o.at) {
                        verr(format!("onsets[{j}]: duplicate onset at pulse {}", o.at));
                    } else {
                        onsets.push(o);
                    }
                }
            }
        }
        Some(_) => verr("onsets: must be an array".into()),
    }
    onsets.sort_by_key(|o| o.at);

    Some(Voice {
        name: name?,
        instrument: instrument?,
        pulses: pulses?,
        onsets,
        gain: gain? as f32,
        pan: pan? as f32,
        mute: mute?,
        keys: keys?,
        silent: silent?,
    })
}

/// Parses a voice's `keys`: a key name for the instrument's default tone,
/// or an object of tone letter → key name. Returns the bindings in the
/// instrument's tone order. `None` (with errors pushed) on any problem,
/// including when the instrument is unknown.
fn parse_keys(
    v: &Value,
    instrument: Option<Instrument>,
    n: usize,
    bound: &mut HashMap<String, usize>,
    err: &mut impl FnMut(String),
) -> Option<Vec<KeyBinding>> {
    let shape = "keys: must be a key name or an object of tone → key name";
    // (tone as written, key name); the tone is `None` for the string form.
    let pairs: Vec<(Option<&str>, &str)> = match v {
        Value::String(k) => vec![(None, k.as_str())],
        Value::Object(obj) => {
            let mut pairs = Vec::new();
            for (t, k) in obj {
                match k.as_str() {
                    Some(k) => pairs.push((Some(t.as_str()), k)),
                    None => {
                        err(shape.into());
                        return None;
                    }
                }
            }
            pairs
        }
        _ => {
            err(shape.into());
            return None;
        }
    };
    let mut ok = true;
    let mut bindings = Vec::new();
    for (tone, key) in pairs {
        let tone = match (tone, instrument) {
            (_, None) => None,
            (None, Some(i)) => Some(i.default_tone()),
            (Some(t), Some(i)) => match i.parse_tone(t) {
                Ok(c) => Some(c),
                Err(m) => {
                    err(format!("keys: {m}"));
                    ok = false;
                    None
                }
            },
        };
        if keys::code(key).is_none() {
            let known: Vec<&str> = keys::names().collect();
            err(format!(
                "keys: unknown key \"{key}\" (known: {})",
                known.join(", ")
            ));
            ok = false;
            continue;
        }
        if let Some(other) = bindings.iter().find(|b: &&KeyBinding| b.key == key) {
            err(format!(
                "keys: key \"{key}\" is also used by tone \"{}\"",
                other.tone
            ));
            ok = false;
            continue;
        }
        match bound.get(key) {
            Some(m) if *m != n => {
                err(format!("keys: key \"{key}\" is also used by voice {m}"));
                ok = false;
                continue;
            }
            _ => {
                bound.insert(key.to_string(), n);
            }
        }
        if let Some(tone) = tone {
            bindings.push(KeyBinding {
                tone,
                key: key.to_string(),
            });
        }
    }
    let instrument = instrument?;
    if !ok {
        return None;
    }
    let order = instrument.tones();
    bindings.sort_by_key(|b| order.iter().position(|t| *t == b.tone));
    Some(bindings)
}

fn parse_onset(
    v: &Value,
    j: usize,
    instrument: Option<Instrument>,
    pulses: Option<u32>,
    err: &mut impl FnMut(String),
) -> Option<Onset> {
    let field = format!("onsets[{j}]");
    let (at, tone, accent) = match v {
        Value::Object(obj) => {
            unknown_keys(obj, ONSET_KEYS, &mut |m| err(format!("{field}: {m}")));
            let at = match obj.get("at") {
                None => {
                    err(format!("{field}: at: missing"));
                    return None;
                }
                Some(a) => a,
            };
            let tone = match obj.get("tone") {
                None => None,
                Some(Value::String(s)) => Some(s.clone()),
                Some(_) => {
                    err(format!("{field}: tone: must be a one-letter string"));
                    return None;
                }
            };
            let accent = opt_bool(obj, "accent", false, &mut |m| err(format!("{field}: {m}")))?;
            (at, tone, accent)
        }
        other => (other, None, false),
    };
    let Some(at) = at.as_u64() else {
        err(format!(
            "{field}: onset must be a non-negative integer pulse index or an object with \"at\""
        ));
        return None;
    };
    if let Some(p) = pulses
        && at >= u64::from(p)
    {
        err(format!("{field}: {at} is not in 0..{p} (pulses is {p})"));
        return None;
    }
    let instrument = instrument?;
    let tone = match tone {
        None => instrument.default_tone(),
        Some(s) => match instrument.parse_tone(&s) {
            Ok(c) => c,
            Err(m) => {
                err(format!("{field}: {m}"));
                return None;
            }
        },
    };
    Some(Onset {
        at: at as u32,
        tone,
        accent,
    })
}

fn unknown_keys(obj: &Map<String, Value>, known: &[&str], err: &mut impl FnMut(String)) {
    for k in obj.keys() {
        if !known.contains(&k.as_str()) {
            err(format!(
                "unknown field \"{k}\" (known: {})",
                known.join(", ")
            ));
        }
    }
}

fn opt_str(obj: &Map<String, Value>, key: &str, err: &mut impl FnMut(String)) -> Option<String> {
    match obj.get(key) {
        None => None,
        Some(Value::String(s)) => Some(s.clone()),
        Some(_) => {
            err(format!("{key}: must be a string"));
            None
        }
    }
}

/// An integer in `min..=max`; `max == u32::MAX` means unbounded above.
fn uint(v: &Value, key: &str, min: u32, max: u32, err: &mut impl FnMut(String)) -> Option<u32> {
    match v.as_u64().and_then(|n| u32::try_from(n).ok()) {
        Some(n) if (min..=max).contains(&n) => Some(n),
        _ if max == u32::MAX => {
            err(format!("{key}: must be an integer >= {min}, got {v}"));
            None
        }
        _ => {
            err(format!(
                "{key}: must be an integer in {min}..={max}, got {v}"
            ));
            None
        }
    }
}

fn req_uint(
    obj: &Map<String, Value>,
    key: &str,
    min: u32,
    max: u32,
    err: &mut impl FnMut(String),
) -> Option<u32> {
    match obj.get(key) {
        None => {
            err(format!("{key}: missing"));
            None
        }
        Some(v) => uint(v, key, min, max, err),
    }
}

fn opt_uint(
    obj: &Map<String, Value>,
    key: &str,
    min: u32,
    max: u32,
    err: &mut impl FnMut(String),
) -> Option<u32> {
    obj.get(key).and_then(|v| uint(v, key, min, max, err))
}

fn opt_range(
    obj: &Map<String, Value>,
    key: &str,
    lo: f64,
    hi: f64,
    default: f64,
    err: &mut impl FnMut(String),
) -> Option<f64> {
    match obj.get(key) {
        None => Some(default),
        Some(v) => match v.as_f64() {
            Some(x) if (lo..=hi).contains(&x) => Some(x),
            _ => {
                err(format!("{key}: must be a number in {lo}..={hi}, got {v}"));
                None
            }
        },
    }
}

/// A string-valued field naming one of `words`: absent reads as the
/// default; anything else is `{key}: must be "a", "b" or "c"`.
fn opt_word<T: Copy + Default>(
    obj: &Map<String, Value>,
    key: &str,
    words: &[T],
    name: fn(T) -> &'static str,
    err: &mut impl FnMut(String),
) -> T {
    let Some(v) = obj.get(key) else {
        return T::default();
    };
    if let Some(&w) = words.iter().find(|&&w| v == name(w)) {
        return w;
    }
    let quoted: Vec<String> = words.iter().map(|&w| format!("\"{}\"", name(w))).collect();
    let list = match quoted.split_last() {
        Some((last, [])) => last.clone(),
        Some((last, rest)) => format!("{} or {last}", rest.join(", ")),
        None => String::new(),
    };
    err(format!("{key}: must be {list}"));
    T::default()
}

/// Sets `obj[key]` to `v`'s word unless `v` is the default.
fn put_word<T: Copy + Default + PartialEq>(
    obj: &mut Value,
    key: &str,
    v: T,
    name: fn(T) -> &'static str,
) {
    if v != T::default() {
        obj[key] = name(v).into();
    }
}

fn opt_bool(
    obj: &Map<String, Value>,
    key: &str,
    default: bool,
    err: &mut impl FnMut(String),
) -> Option<bool> {
    match obj.get(key) {
        None => Some(default),
        Some(Value::Bool(b)) => Some(*b),
        Some(_) => {
            err(format!("{key}: must be true or false"));
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn parse(v: Value) -> (Option<Exercise>, Diagnostics) {
        let mut d = Diagnostics::default();
        let e = Exercise::from_value(&v, 1, "", &mut d);
        (e, d)
    }

    fn example() -> Value {
        json!({
          "label": "A",
          "title": "Son clave 2-3 against the bell",
          "tempo": 90,
          "beats": 4,
          "cycles": 8,
          "count_in": 4,
          "voices": [
            { "name": "clave", "instrument": "clave", "pulses": 16,
              "onsets": [2, 4, 8, 11, 14] },
            { "name": "bell",  "instrument": "bell",  "pulses": 12,
              "onsets": [0, 2, 4, 5, 7, 9, 11], "gain": 0.7, "pan": -0.4 },
            { "name": "pulse", "instrument": "click", "pulses": 4,
              "onsets": [{"at": 0, "accent": true}, 1, 2, 3], "gain": 0.4 }
          ]
        })
    }

    /// Asserts exactly one error, containing every fragment.
    fn one_error(v: Value, fragments: &[&str]) {
        let (e, d) = parse(v);
        assert!(e.is_none());
        assert_eq!(d.errors.len(), 1, "{:?}", d.errors);
        for f in fragments {
            assert!(d.errors[0].contains(f), "{:?} lacks {f:?}", d.errors[0]);
        }
    }

    fn with_voice(voice: Value) -> Value {
        json!({ "tempo": 90, "beats": 4, "voices": [voice] })
    }

    #[test]
    fn parses_example() {
        let (e, d) = parse(example());
        assert!(d.errors.is_empty() && d.warnings.is_empty(), "{d:?}");
        let e = e.unwrap();
        assert_eq!(e.label, "A");
        assert_eq!(e.title.as_deref(), Some("Son clave 2-3 against the bell"));
        assert_eq!(
            (e.tempo, e.beats, e.cycles, e.count_in),
            (90.0, 4, Some(8), 4)
        );
        assert_eq!(e.voices.len(), 3);
        let bell = &e.voices[1];
        assert_eq!(bell.instrument, Instrument::Bell);
        assert_eq!(bell.pulses, 12);
        assert_eq!((bell.gain, bell.pan, bell.mute), (0.7, -0.4, false));
        assert!(bell.onsets.iter().all(|o| o.tone == 'h' && !o.accent));
        let pulse = &e.voices[2];
        assert!(pulse.onsets[0].accent);
        assert!(!pulse.onsets[1].accent);
    }

    #[test]
    fn integer_and_object_onsets_are_equivalent() {
        let a = parse(with_voice(json!({"name": "c", "instrument": "conga",
            "pulses": 4, "onsets": [0, 2]})))
        .0
        .unwrap();
        let b = parse(with_voice(json!({"name": "c", "instrument": "conga",
            "pulses": 4, "onsets": [{"at": 0}, {"at": 2, "tone": "o", "accent": false}]})))
        .0
        .unwrap();
        assert_eq!(a, b);
        assert_eq!(a.voices[0].onsets[0].tone, 'o');
    }

    #[test]
    fn defaults_applied() {
        let e = parse(with_voice(json!({"name": "c", "instrument": "clave",
            "pulses": 4, "onsets": [0]})))
        .0
        .unwrap();
        assert_eq!(e.label, "block 1");
        assert_eq!((e.cycles, e.count_in), (None, 0));
        let v = &e.voices[0];
        assert_eq!((v.gain, v.pan, v.mute), (1.0, 0.0, false));
    }

    #[test]
    fn label_falls_back_to_title() {
        let mut v = example();
        v.as_object_mut().unwrap().remove("label");
        assert_eq!(parse(v).0.unwrap().label, "Son clave 2-3 against the bell");
    }

    #[test]
    fn gm_instrument_bounds() {
        let gm =
            |s: &str| with_voice(json!({"name": "g", "instrument": s, "pulses": 2, "onsets": [0]}));
        let e = parse(gm("gm:35")).0.unwrap();
        assert_eq!(e.voices[0].instrument, Instrument::Gm(35));
        assert_eq!(Instrument::Gm(35).gm_key('x'), Some(35));
        assert!(parse(gm("gm:81")).0.is_some());
        one_error(gm("gm:34"), &["voice 1", "instrument", "35..=81"]);
        one_error(gm("gm:82"), &["voice 1", "instrument", "35..=81"]);
        one_error(gm("gm:xx"), &["voice 1", "unknown instrument"]);
    }

    #[test]
    fn instrument_table() {
        for i in Instrument::named() {
            assert_eq!(Instrument::parse(&i.name()), Ok(i));
            for t in i.tones() {
                assert!(i.gm_key(t).is_some());
            }
        }
        assert_eq!(Instrument::Conga.tones(), vec!['o', 's', 'b']);
        assert_eq!(Instrument::Conga.gm_key('s'), Some(62));
        assert_eq!(Instrument::Bell.gm_key('l'), Some(68));
        assert_eq!(Instrument::Clave.gm_key('h'), None);
    }

    #[test]
    fn onset_out_of_range() {
        one_error(
            with_voice(
                json!({"name": "c", "instrument": "clave", "pulses": 16, "onsets": [2, 16]}),
            ),
            &["voice 1", "onsets[1]", "0..16"],
        );
    }

    #[test]
    fn duplicate_onsets() {
        one_error(
            with_voice(json!({"name": "c", "instrument": "clave", "pulses": 4,
                "onsets": [1, {"at": 1, "accent": true}]})),
            &["voice 1", "onsets[1]", "duplicate"],
        );
    }

    #[test]
    fn unknown_instrument_and_tone() {
        one_error(
            with_voice(json!({"name": "c", "instrument": "kazoo", "pulses": 4, "onsets": [0]})),
            &["voice 1", "instrument", "kazoo"],
        );
        one_error(
            with_voice(json!({"name": "c", "instrument": "bell", "pulses": 4,
                "onsets": [{"at": 0, "tone": "o"}]})),
            &["voice 1", "onsets[0]", "tone \"o\"", "hl"],
        );
        one_error(
            with_voice(json!({"name": "c", "instrument": "bell", "pulses": 4,
                "onsets": [{"at": 0, "tone": "hl"}]})),
            &["voice 1", "onsets[0]", "tone"],
        );
    }

    #[test]
    fn zero_pulses() {
        one_error(
            with_voice(json!({"name": "c", "instrument": "clave", "pulses": 0, "onsets": []})),
            &["voice 1", "pulses"],
        );
    }

    #[test]
    fn upper_bounds() {
        let mut v = example();
        v["beats"] = json!(MAX_BEATS + 1);
        one_error(v, &["beats: must be an integer in 1..=64, got 65"]);
        let mut v = example();
        v["count_in"] = json!(u32::MAX);
        one_error(v, &["count_in: must be an integer in 0..=64"]);
        one_error(
            with_voice(
                json!({"name": "c", "instrument": "clave", "pulses": MAX_PULSES + 1,
                "onsets": [0]}),
            ),
            &["voice 1: pulses: must be an integer in 1..=1024, got 1025"],
        );
        let mut v = example();
        v["beats"] = json!(MAX_BEATS);
        v["count_in"] = json!(MAX_COUNT_IN);
        v["cycles"] = json!(u32::MAX);
        v["voices"][0]["pulses"] = json!(MAX_PULSES);
        assert!(parse(v).0.is_some());
    }

    #[test]
    fn voice_count_bounds() {
        one_error(
            json!({"tempo": 90, "beats": 4, "voices": []}),
            &["voices", "at least one"],
        );
        let v = json!({"name": "c", "instrument": "clave", "pulses": 1, "onsets": [0]});
        let ten: Vec<Value> = (0..10).map(|_| v.clone()).collect();
        one_error(
            json!({"tempo": 90, "beats": 4, "voices": ten}),
            &["voices", "at most 9"],
        );
        let nine: Vec<Value> = (0..9).map(|_| v.clone()).collect();
        assert!(
            parse(json!({"tempo": 90, "beats": 4, "voices": nine}))
                .0
                .is_some()
        );
    }

    #[test]
    fn gain_pan_mute_types() {
        let voice = |k: &str, x: Value| {
            let mut v = json!({"name": "c", "instrument": "clave", "pulses": 1, "onsets": [0]});
            v[k] = x;
            with_voice(v)
        };
        one_error(voice("gain", json!(1.5)), &["voice 1", "gain"]);
        one_error(voice("pan", json!(-2)), &["voice 1", "pan"]);
        one_error(voice("mute", json!("yes")), &["voice 1", "mute"]);
        assert!(parse(voice("mute", json!(true))).0.unwrap().voices[0].mute);
    }

    #[test]
    fn every_error_is_reported() {
        let (e, d) = parse(json!({
            "tempo": "fast",
            "beats": 0,
            "voices": [
                { "name": "a", "instrument": "clave", "pulses": 4, "onsets": [4] },
                { "name": "b", "instrument": "nope", "pulses": 0, "onsets": [] }
            ]
        }));
        assert!(e.is_none());
        let all = d.errors.join("\n");
        for f in [
            "tempo",
            "beats",
            "voice 1: onsets[0]",
            "voice 2: instrument",
            "voice 2: pulses",
        ] {
            assert!(all.contains(f), "{all} lacks {f}");
        }
        assert_eq!(d.errors.len(), 5, "{all}");
    }

    #[test]
    fn hidden_must_be_a_boolean() {
        assert!(!parse(example()).0.unwrap().hidden, "default false");
        let mut v = example();
        v["hidden"] = json!(1);
        one_error(v, &["hidden: must be true or false"]);
        let mut v = example();
        v["hidden"] = json!(true);
        assert!(parse(v).0.unwrap().hidden);
    }

    #[test]
    fn unknown_fields_are_errors_except_source() {
        let mut v = example();
        v["count-in"] = json!(4);
        one_error(v, &["unknown field \"count-in\""]);
        let (_, d) = parse({
            let mut v = example();
            v["hide"] = json!(true);
            v
        });
        assert!(
            d.errors[0].contains("count_in, hidden, window_ms, voices"),
            "{:?}",
            d.errors
        );
        assert!(
            d.errors[0].contains("playhead, feedback, cycle_count"),
            "{:?}",
            d.errors
        );
        let mut v = example();
        v["source"] = json!("https://example.org");
        assert!(parse(v).0.is_some());
    }

    #[test]
    fn tempo_clamped_with_warning() {
        let mut v = example();
        v["tempo"] = json!(400);
        let (e, d) = parse(v);
        assert_eq!(e.unwrap().tempo, 300.0);
        assert_eq!(d.warnings.len(), 1);
        assert!(d.warnings[0].contains("clamped"));
        let mut v = example();
        v["tempo"] = json!(5);
        assert_eq!(parse(v).0.unwrap().tempo, 20.0);
    }

    #[test]
    fn missing_required_fields() {
        let (e, d) = parse(json!({}));
        assert!(e.is_none());
        let all = d.errors.join("\n");
        for f in ["tempo: missing", "beats: missing", "voices: missing"] {
            assert!(all.contains(f), "{all} lacks {f}");
        }
    }

    #[test]
    fn serde_form_is_authoring_json() {
        let mut v = example();
        v.as_object_mut().unwrap().remove("title");
        v.as_object_mut().unwrap().remove("cycles");
        let (e, _) = parse(v);
        let e = e.unwrap();
        let wire = serde_json::to_value(&e).unwrap();
        assert!(wire.get("title").is_none() && wire.get("cycles").is_none());
        let (back, d) = parse(wire.clone());
        assert!(d.errors.is_empty(), "{:?}", d.errors);
        assert_eq!(back.unwrap(), e);
        assert_eq!(serde_json::from_value::<Exercise>(wire).unwrap(), e);
        let mut hidden = example();
        hidden["hidden"] = json!(true);
        let (full, _) = parse(hidden);
        let full = full.unwrap();
        assert!(full.hidden);
        let s = serde_json::to_string(&full).unwrap();
        assert_eq!(serde_json::from_str::<Exercise>(&s).unwrap(), full);
    }

    fn clave(keys: Value) -> Value {
        json!({"name": "c", "instrument": "clave", "pulses": 4, "onsets": [0], "keys": keys})
    }

    fn conga(keys: Value) -> Value {
        json!({"name": "g", "instrument": "conga", "pulses": 4, "onsets": [0], "keys": keys})
    }

    #[test]
    fn keys_string_binds_the_default_tone() {
        let e = parse(with_voice(clave(json!("f")))).0.unwrap();
        assert_eq!(
            e.voices[0].keys,
            vec![KeyBinding {
                tone: 'x',
                key: "f".into()
            }]
        );
        let e = parse(with_voice(conga(json!("comma")))).0.unwrap();
        assert_eq!(e.voices[0].keys[0].tone, 'o');
    }

    #[test]
    fn keys_object_binds_tones_in_instrument_order() {
        let e = parse(with_voice(conga(
            json!({"b": "l", "o": "f", "s": "semicolon"}),
        )));
        let keys = &e.0.unwrap().voices[0].keys;
        let pairs: Vec<(char, &str)> = keys.iter().map(|b| (b.tone, b.key.as_str())).collect();
        assert_eq!(pairs, [('o', "f"), ('s', "semicolon"), ('b', "l")]);
    }

    #[test]
    fn keys_errors() {
        one_error(
            with_voice(clave(json!(3))),
            &["voice 1: keys: must be a key name or an object of tone → key name"],
        );
        one_error(
            with_voice(clave(json!({"x": 1}))),
            &["voice 1: keys: must be a key name or an object"],
        );
        one_error(
            with_voice(clave(json!("F"))),
            &[
                "voice 1: keys: unknown key \"F\" (known: a, b, c,",
                "grave)",
            ],
        );
        for control in ["h", "j", "k", "q", "r", "1", "space", "tab", "enter"] {
            one_error(
                with_voice(clave(json!(control))),
                &[&format!("voice 1: keys: unknown key \"{control}\"")],
            );
        }
        one_error(
            with_voice(conga(json!({"x": "f"}))),
            &["voice 1: keys: tone \"x\" is not a tone of conga (tones: osb)"],
        );
        one_error(
            with_voice(conga(json!({"o": "f", "s": "f"}))),
            &["voice 1: keys: key \"f\" is also used by tone"],
        );
        let (e, d) = parse(with_voice(conga(json!({"o": "space", "x": "f"}))));
        assert!(e.is_none());
        assert_eq!(d.errors.len(), 2, "{:?}", d.errors);
    }

    #[test]
    fn a_key_maps_to_one_voice() {
        let (e, d) = parse(json!({ "tempo": 90, "beats": 4, "voices": [
            clave(json!("f")),
            conga(json!({"o": "d"})),
            conga(json!({"o": "g", "s": "f"}))
        ]}));
        assert!(e.is_none());
        assert_eq!(
            d.errors,
            ["voice 3: keys: key \"f\" is also used by voice 1"]
        );
    }

    #[test]
    fn keys_make_the_voice_a_muted_player_part() {
        let e = parse(json!({ "tempo": 90, "beats": 4, "voices": [
            clave(json!("f")),
            {"name": "p", "instrument": "click", "pulses": 4, "onsets": [0]},
            {"name": "g", "instrument": "conga", "pulses": 4, "onsets": [0],
             "keys": "d", "mute": false},
            {"name": "m", "instrument": "click", "pulses": 4, "onsets": [0], "mute": true}
        ]}))
        .0
        .unwrap();
        let mutes: Vec<bool> = e.voices.iter().map(|v| v.mute).collect();
        assert_eq!(mutes, [true, false, false, true]);
        assert_eq!(e.player_voices().collect::<Vec<_>>(), [0, 2]);
        assert_eq!(parse(example()).0.unwrap().player_voices().count(), 0);
    }

    #[test]
    fn silent_is_a_flag_of_a_player_voice() {
        let mut v = clave(json!("f"));
        assert!(!parse(with_voice(v.clone())).0.unwrap().voices[0].silent);
        v["silent"] = json!(true);
        assert!(parse(with_voice(v.clone())).0.unwrap().voices[0].silent);
        v["silent"] = json!("yes");
        one_error(with_voice(v), &["voice 1: silent: must be true or false"]);
        let plain = json!({"name": "c", "instrument": "clave", "pulses": 4, "onsets": [0],
                           "silent": true});
        one_error(
            json!({"tempo": 90, "beats": 4, "voices": [clave(json!("f")), plain]}),
            &["voice 2: silent: only a voice with keys has strokes to silence"],
        );
        let mut off = clave(json!("f"));
        off["silent"] = json!(false);
        let plain = json!({"name": "c", "instrument": "clave", "pulses": 4, "onsets": [0],
                           "silent": false});
        assert!(
            parse(json!({"tempo": 90, "beats": 4, "voices": [off, plain]}))
                .0
                .is_some()
        );
    }

    #[test]
    fn silent_reaches_the_wire_only_when_true() {
        let mut v = example();
        v["voices"][0]["keys"] = json!("f");
        v["voices"][0]["silent"] = json!(true);
        v["voices"][1]["keys"] = json!("d");
        let e = parse(v).0.unwrap();
        let wire = serde_json::to_value(&e).unwrap();
        assert_eq!(wire["voices"][0]["silent"], json!(true));
        assert!(wire["voices"][1].get("silent").is_none());
        assert!(wire["voices"][2].get("silent").is_none());
        assert_eq!(serde_json::from_value::<Exercise>(wire).unwrap(), e);
    }

    #[test]
    fn ending_parses_and_defaults_to_downbeat() {
        assert_eq!(parse(example()).0.unwrap().ending, Ending::Downbeat);
        for (w, e) in [("downbeat", Ending::Downbeat), ("none", Ending::None)] {
            let mut v = example();
            v["ending"] = json!(w);
            assert_eq!(parse(v).0.unwrap().ending, e);
        }
    }

    #[test]
    fn ending_rejects_other_values() {
        for bad in [json!("stop"), json!(true), json!(null)] {
            let mut v = example();
            v["ending"] = bad;
            one_error(v, &["ending: must be \"downbeat\" or \"none\""]);
        }
    }

    #[test]
    fn exercise_json_omits_default_ending() {
        let plain = serde_json::to_value(parse(example()).0.unwrap()).unwrap();
        assert!(plain.get("ending").is_none());
        let mut v = example();
        v["ending"] = json!("none");
        let e = parse(v).0.unwrap();
        let wire = serde_json::to_value(&e).unwrap();
        assert_eq!(wire["ending"], json!("none"));
        assert_eq!(serde_json::from_value::<Exercise>(wire).unwrap(), e);
    }

    #[test]
    fn next_parses_and_defaults_to_stop() {
        assert_eq!(parse(example()).0.unwrap().next, Next::Stop);
        for (w, n) in [("stop", Next::Stop), ("attacca", Next::Attacca)] {
            let mut v = example();
            v["next"] = json!(w);
            assert_eq!(parse(v).0.unwrap().next, n);
        }
    }

    #[test]
    fn next_rejects_other_values() {
        for bad in [json!("B"), json!(true), json!(null), json!({"when": "key"})] {
            let mut v = example();
            v["next"] = bad;
            one_error(v, &["next: must be \"stop\" or \"attacca\""]);
        }
    }

    #[test]
    fn exercise_json_omits_default_next() {
        let plain = serde_json::to_value(parse(example()).0.unwrap()).unwrap();
        assert!(plain.get("next").is_none());
        let mut v = example();
        v["next"] = json!("attacca");
        let e = parse(v).0.unwrap();
        let wire = serde_json::to_value(&e).unwrap();
        assert_eq!(wire["next"], json!("attacca"));
        assert_eq!(serde_json::from_value::<Exercise>(wire).unwrap(), e);
    }

    #[test]
    fn playhead_parses_and_defaults_to_true() {
        assert!(parse(example()).0.unwrap().playhead, "default true");
        let mut v = example();
        v["playhead"] = json!(false);
        assert!(!parse(v).0.unwrap().playhead);
        let mut v = example();
        v["playhead"] = json!("no");
        one_error(v, &["playhead: must be true or false"]);
    }

    #[test]
    fn exercise_json_omits_default_playhead() {
        let plain = serde_json::to_value(parse(example()).0.unwrap()).unwrap();
        assert!(plain.get("playhead").is_none());
        let mut v = example();
        v["playhead"] = json!(false);
        let e = parse(v).0.unwrap();
        let wire = serde_json::to_value(&e).unwrap();
        assert_eq!(wire["playhead"], json!(false));
        assert_eq!(serde_json::from_value::<Exercise>(wire).unwrap(), e);
    }

    #[test]
    fn feedback_parses_and_defaults_to_live() {
        assert_eq!(parse(example()).0.unwrap().feedback, Feedback::Live);
        for (w, f) in [
            ("live", Feedback::Live),
            ("after", Feedback::After),
            ("none", Feedback::None),
        ] {
            let mut v = example();
            v["feedback"] = json!(w);
            assert_eq!(parse(v).0.unwrap().feedback, f);
        }
        for bad in [json!("later"), json!(false), json!(1), json!(null)] {
            let mut v = example();
            v["feedback"] = bad;
            one_error(v, &["feedback: must be \"live\", \"after\" or \"none\""]);
        }
    }

    #[test]
    fn cycle_count_parses_and_defaults_to_count() {
        assert_eq!(parse(example()).0.unwrap().cycle_count, CycleCount::Count);
        for (w, c) in [
            ("countdown", CycleCount::Countdown),
            ("count", CycleCount::Count),
            ("none", CycleCount::None),
        ] {
            let mut v = example();
            v["cycle_count"] = json!(w);
            assert_eq!(parse(v).0.unwrap().cycle_count, c);
        }
        for bad in [json!("down"), json!(true), json!(3), json!(null)] {
            let mut v = example();
            v["cycle_count"] = bad;
            one_error(
                v,
                &["cycle_count: must be \"countdown\", \"count\" or \"none\""],
            );
        }
    }

    #[test]
    fn countdown_needs_cycles() {
        let mut v = example();
        v.as_object_mut().unwrap().remove("cycles");
        v["cycle_count"] = json!("countdown");
        let (e, d) = parse(v);
        assert!(e.is_none());
        assert_eq!(
            d.errors,
            ["cycle_count: \"countdown\" needs cycles (a looping block never ends)"]
        );
        for ok in ["count", "none"] {
            let mut v = example();
            v.as_object_mut().unwrap().remove("cycles");
            v["cycle_count"] = json!(ok);
            assert_eq!(parse(v).0.unwrap().cycles, None);
        }
        let mut v = example();
        v["cycles"] = json!(0);
        v["cycle_count"] = json!("countdown");
        one_error(v, &["cycles: "]);
    }

    #[test]
    fn exercise_json_omits_default_feedback_and_cycle_count() {
        let plain = serde_json::to_value(parse(example()).0.unwrap()).unwrap();
        assert!(plain.get("feedback").is_none());
        assert!(plain.get("cycle_count").is_none());
        for (key, w) in [
            ("feedback", "after"),
            ("feedback", "none"),
            ("cycle_count", "countdown"),
            ("cycle_count", "none"),
        ] {
            let mut v = example();
            v[key] = json!(w);
            let e = parse(v).0.unwrap();
            let wire = serde_json::to_value(&e).unwrap();
            assert_eq!(wire[key], json!(w));
            assert_eq!(serde_json::from_value::<Exercise>(wire).unwrap(), e);
        }
    }

    #[test]
    fn window_ms_bounds_and_default() {
        assert_eq!(parse(example()).0.unwrap().window_ms(), DEFAULT_WINDOW_MS);
        for w in [10.0, 150.0, 500.0] {
            let mut v = example();
            v["window_ms"] = json!(w);
            let e = parse(v).0.unwrap();
            assert_eq!((e.window_ms, e.window_ms()), (Some(w), w));
        }
        for bad in [json!(9.9), json!(501), json!("100")] {
            let mut v = example();
            v["window_ms"] = bad;
            one_error(v, &["window_ms: must be a number from 10 to 500"]);
        }
    }

    #[test]
    fn serde_round_trip_with_keys_and_window() {
        let mut v = example();
        v["window_ms"] = json!(80.0);
        v["voices"][0]["keys"] = json!("f");
        v["voices"][1]["keys"] = json!({"l": "d", "h": "s"});
        let e = parse(v).0.unwrap();
        let wire = serde_json::to_value(&e).unwrap();
        assert_eq!(wire["window_ms"], json!(80.0));
        assert_eq!(wire["voices"][0]["keys"], json!({"x": "f"}));
        assert_eq!(wire["voices"][1]["keys"], json!({"h": "s", "l": "d"}));
        assert!(wire["voices"][2].get("keys").is_none());
        assert_eq!(serde_json::from_value::<Exercise>(wire).unwrap(), e);
        let plain = serde_json::to_value(parse(example()).0.unwrap()).unwrap();
        assert!(plain.get("window_ms").is_none());
    }

    #[test]
    fn deserializing_validates() {
        let mut v = example();
        v["voices"][0]["pulses"] = json!(0);
        v["voices"][1]["onsets"] = json!([{ "at": 0, "tone": "z" }]);
        let err = serde_json::from_value::<Exercise>(v)
            .unwrap_err()
            .to_string();
        assert!(err.contains("voice 1: pulses"), "{err}");
        assert!(err.contains("voice 2: onsets[0]: tone"), "{err}");
    }
}
