//! The sound bank: GM percussion one-shots pre-rendered through FluidSynth.
//!
//! The bank lives on the UI thread. Its buffers reach the audio thread only
//! as `Arc` clones carried inside a timeline's events.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, bail};

use crate::fluid::Synth;
use crate::model::Instrument;

/// MIDI channel 10 (0-based 9): the GM percussion kit.
const DRUM_CHANNEL: i32 = 9;
const VELOCITY: i32 = 96;
const VELOCITY_ACCENT: i32 = 127;
/// Peak of an instrument's loudest (accented) buffer after level matching.
pub const PEAK: f32 = 0.8;
const CHUNK: usize = 512;
const MIN_SECS: f64 = 0.050;
const TAIL_SECS: f64 = 0.005;
const MAX_SECS: f64 = 2.0;
const FLUSH_SECS: f64 = 0.100;
/// −60 dBFS as linear amplitude.
pub const SILENCE: f32 = 0.001;

pub const DEFAULT_SOUNDFONTS: &[&str] = &[
    "/usr/share/soundfonts/default.sf2",
    "/usr/share/soundfonts/FluidR3_GM.sf2",
    "/usr/share/sounds/sf2/FluidR3_GM.sf2",
];

/// One renderable sound: an instrument's tone, plain or accented.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct SoundId {
    pub instrument: Instrument,
    pub tone: char,
    pub accent: bool,
}

enum Source {
    Synth(Synth),
    #[cfg(test)]
    Fn(Box<dyn FnMut(SoundId) -> Arc<[f32]> + Send>),
}

/// `SoundId` → mono buffer at the device sample rate. Entries are never
/// evicted, so every buffer handed out stays referenced for the life of the
/// bank.
pub struct Bank {
    cache: HashMap<SoundId, Arc<[f32]>>,
    source: Source,
}

impl Bank {
    /// Renders every tone of every named instrument, plain and accented.
    /// Fails if the soundfont has no sound for any of them.
    pub fn new(mut synth: Synth) -> Result<Bank> {
        let mut cache = HashMap::new();
        for inst in Instrument::named() {
            render_instrument(&mut synth, &mut cache, inst)?;
        }
        Ok(Bank {
            cache,
            source: Source::Synth(synth),
        })
    }

    /// A bank with no synth whose `get` calls `f` on a miss.
    #[cfg(test)]
    pub fn from_fn(f: impl FnMut(SoundId) -> Arc<[f32]> + Send + 'static) -> Bank {
        Bank {
            cache: HashMap::new(),
            source: Source::Fn(Box::new(f)),
        }
    }

    /// The cached buffer for `id`, rendering it first if never seen. Fails
    /// if the soundfont has no sound for it.
    pub fn get(&mut self, id: SoundId) -> Result<Arc<[f32]>> {
        if let Some(buf) = self.cache.get(&id) {
            return Ok(buf.clone());
        }
        match &mut self.source {
            #[cfg(test)]
            Source::Fn(f) => {
                let buf = f(id);
                self.cache.insert(id, buf.clone());
                Ok(buf)
            }
            Source::Synth(synth) => {
                // Every named instrument is rendered by `new`, so a miss is a
                // `gm:NN` key never seen before; rendering it adds entries
                // and replaces none.
                render_instrument(synth, &mut self.cache, id.instrument)?;
                Ok(self
                    .cache
                    .get(&id)
                    .expect("validated exercises name only tones their instrument has")
                    .clone())
            }
        }
    }
}

/// Renders all tones of `inst`, plain and accented, scales them by one factor
/// so the loudest peaks at `PEAK`, and adds them to `cache`. Adds nothing if
/// any of them is silent.
fn render_instrument(
    synth: &mut Synth,
    cache: &mut HashMap<SoundId, Arc<[f32]>>,
    inst: Instrument,
) -> Result<()> {
    let soundfont = synth.soundfont().display().to_string();
    let mut rendered = Vec::new();
    for tone in inst.tones() {
        let Some(key) = inst.gm_key(tone) else {
            continue;
        };
        for accent in [false, true] {
            let vel = if accent { VELOCITY_ACCENT } else { VELOCITY };
            let id = SoundId {
                instrument: inst,
                tone,
                accent,
            };
            let no_sound = || {
                format!(
                    "soundfont {soundfont} has no sound for {} tone {tone} (GM percussion key \
                     {key}); use a General MIDI soundfont with a percussion kit, such as \
                     fluid-soundfont-gm",
                    inst.name()
                )
            };
            let buf = render_one(synth, key as i32, vel).with_context(no_sound)?;
            if !buf.iter().any(|s| *s != 0.0) {
                bail!(no_sound());
            }
            rendered.push((id, buf));
        }
    }
    let peak = rendered
        .iter()
        .flat_map(|(_, b)| b.iter())
        .fold(0.0f32, |m, s| m.max(s.abs()));
    let scale = if peak > 0.0 { PEAK / peak } else { 1.0 };
    for (id, mut buf) in rendered {
        for s in &mut buf {
            *s *= scale;
        }
        cache.insert(id, Arc::from(buf));
    }
    Ok(())
}

fn rms(s: &[f32]) -> f32 {
    if s.is_empty() {
        return 0.0;
    }
    (s.iter().map(|x| x * x).sum::<f32>() / s.len() as f32).sqrt()
}

/// One note on the drum channel, rendered until it has decayed below
/// −60 dBFS at the level it will have once scaled to peak at `PEAK` (at
/// least 50 ms, at most 2 s), downmixed to mono. Level matching scales a
/// buffer by at most `PEAK / its own peak`, so the tail stays below −60 dBFS
/// after scaling; a note that makes no sound stops at 50 ms. The synth is
/// left silent for the next note.
fn render_one(synth: &mut Synth, key: i32, vel: i32) -> Result<Vec<f32>> {
    let rate = synth.sample_rate() as f64;
    let min = (MIN_SECS * rate) as usize;
    let tail = ((TAIL_SECS * rate) as usize).max(1);
    let max = (MAX_SECS * rate) as usize;
    let mut out = Vec::new();
    let mut peak = 0.0f32;
    synth.noteon(DRUM_CHANNEL, key, vel)?;
    loop {
        let (l, r) = synth.render(CHUNK);
        let start = out.len();
        out.extend(l.iter().zip(&r).map(|(l, r)| (l + r) / 2.0));
        peak = out[start..].iter().fold(peak, |m, s| m.max(s.abs()));
        if out.len() >= max {
            break;
        }
        if out.len() >= min && rms(&out[out.len() - tail..]) <= SILENCE * peak / PEAK {
            break;
        }
    }
    synth.noteoff(DRUM_CHANNEL, key);
    synth.all_sounds_off();
    let _ = synth.render((FLUSH_SECS * rate) as usize);
    fade_out(&mut out, tail);
    Ok(out)
}

/// Linear fade to zero over the last `len` samples, so a sound cut at the
/// render limit ends without a click.
fn fade_out(buf: &mut [f32], len: usize) {
    let len = len.min(buf.len());
    let start = buf.len() - len;
    for (i, s) in buf[start..].iter_mut().enumerate() {
        *s *= 1.0 - (i + 1) as f32 / len as f32;
    }
}

/// The soundfont `djembot gui` uses: `--soundfont` if given, else
/// `$DJEMBOT_SOUNDFONT` if set and non-empty, else the first existing
/// default. A named file must exist; nothing else is tried for it.
pub fn find_soundfont(cli: Option<&Path>) -> Result<PathBuf> {
    let env = std::env::var("DJEMBOT_SOUNDFONT").ok();
    choose_soundfont(cli, env.as_deref(), DEFAULT_SOUNDFONTS)
}

fn choose_soundfont(cli: Option<&Path>, env: Option<&str>, defaults: &[&str]) -> Result<PathBuf> {
    if let Some(path) = cli {
        if !path.is_file() {
            bail!("--soundfont {}: no such file", path.display());
        }
        return Ok(path.to_path_buf());
    }
    if let Some(value) = env.filter(|v| !v.is_empty()) {
        let path = PathBuf::from(value);
        if !path.is_file() {
            bail!("DJEMBOT_SOUNDFONT={value}: no such file");
        }
        return Ok(path);
    }
    if let Some(p) = defaults.iter().map(PathBuf::from).find(|p| p.is_file()) {
        return Ok(p);
    }
    bail!(
        "no soundfont found (tried {}); install fluid-soundfont-gm or pass --soundfont PATH",
        defaults.join(", ")
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[test]
    fn from_fn_caches_misses() {
        let calls = Arc::new(Mutex::new(0));
        let c = calls.clone();
        let mut bank = Bank::from_fn(move |_| {
            *c.lock().unwrap() += 1;
            Arc::from(vec![1.0f32])
        });
        let id = SoundId {
            instrument: Instrument::Clave,
            tone: 'x',
            accent: false,
        };
        let a = bank.get(id).unwrap();
        let b = bank.get(id).unwrap();
        assert!(Arc::ptr_eq(&a, &b));
        assert_eq!(*calls.lock().unwrap(), 1);
        bank.get(SoundId { accent: true, ..id }).unwrap();
        assert_eq!(*calls.lock().unwrap(), 2);
    }

    #[test]
    fn fade_out_ends_at_zero() {
        let mut buf = vec![1.0f32; 10];
        fade_out(&mut buf, 4);
        assert_eq!(&buf[..6], &[1.0; 6]);
        assert_eq!(buf[9], 0.0);
        assert!(buf[6] > buf[7] && buf[7] > buf[8]);
    }

    /// A directory holding `names` as empty files.
    fn soundfont_dir(test: &str, names: &[&str]) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("djembot-sf-{}-{test}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for n in names {
            std::fs::write(dir.join(n), b"").unwrap();
        }
        dir
    }

    fn s(p: &Path) -> String {
        p.display().to_string()
    }

    #[test]
    fn named_soundfont_must_exist() {
        let dir = soundfont_dir("named", &["a.sf2", "default.sf2"]);
        let a = dir.join("a.sf2");
        let missing = dir.join("typo.sf2");
        let defaults = [s(&dir.join("default.sf2"))];
        let defaults: Vec<&str> = defaults.iter().map(String::as_str).collect();

        // The flag wins over the variable, and a missing flag path is final.
        let got = choose_soundfont(Some(&a), Some(&s(&missing)), &defaults).unwrap();
        assert_eq!(got, a);
        let err = choose_soundfont(Some(&missing), Some(&s(&a)), &defaults)
            .unwrap_err()
            .to_string();
        assert_eq!(
            err,
            format!("--soundfont {}: no such file", missing.display())
        );

        // The variable, when set and non-empty, is final too.
        let got = choose_soundfont(None, Some(&s(&a)), &defaults).unwrap();
        assert_eq!(got, a);
        let err = choose_soundfont(None, Some(&s(&missing)), &defaults)
            .unwrap_err()
            .to_string();
        assert_eq!(
            err,
            format!("DJEMBOT_SOUNDFONT={}: no such file", missing.display())
        );
        let got = choose_soundfont(None, Some(""), &defaults).unwrap();
        assert_eq!(got, dir.join("default.sf2"));
    }

    #[test]
    fn defaults_are_searched_in_order() {
        let dir = soundfont_dir("defaults", &["second.sf2", "third.sf2"]);
        let paths = ["first.sf2", "second.sf2", "third.sf2"].map(|n| s(&dir.join(n)));
        let defaults: Vec<&str> = paths.iter().map(String::as_str).collect();
        let got = choose_soundfont(None, None, &defaults).unwrap();
        assert_eq!(got, dir.join("second.sf2"));
        assert_eq!(
            DEFAULT_SOUNDFONTS,
            [
                "/usr/share/soundfonts/default.sf2",
                "/usr/share/soundfonts/FluidR3_GM.sf2",
                "/usr/share/sounds/sf2/FluidR3_GM.sf2",
            ]
        );
    }

    #[test]
    fn no_soundfont_error_names_package() {
        let dir = soundfont_dir("none", &[]);
        let paths = ["x.sf2", "y.sf2"].map(|n| s(&dir.join(n)));
        let defaults: Vec<&str> = paths.iter().map(String::as_str).collect();
        let msg = choose_soundfont(None, None, &defaults)
            .unwrap_err()
            .to_string();
        assert!(msg.contains("fluid-soundfont-gm"), "{msg}");
        assert!(msg.contains(&paths[0]) && msg.contains(&paths[1]), "{msg}");
        assert!(msg.contains("--soundfont PATH"), "{msg}");
    }

    /// Needs FluidSynth and the GM soundfont: `cargo test -- --ignored`.
    #[test]
    #[ignore]
    fn real_soundfont_renders_every_entry() {
        let sf = find_soundfont(None).expect("soundfont");
        let rate = 48_000;
        let mut bank = Bank::new(Synth::new(rate, &sf).expect("synth")).expect("bank");
        let tail = (TAIL_SECS * rate as f64) as usize;
        let ids = |insts: Vec<Instrument>| {
            let mut ids: Vec<SoundId> = Vec::new();
            for inst in insts {
                for tone in inst.tones() {
                    for accent in [false, true] {
                        ids.push(SoundId {
                            instrument: inst,
                            tone,
                            accent,
                        });
                    }
                }
            }
            ids
        };
        let peak = |buf: &[f32]| buf.iter().fold(0.0f32, |m, s| m.max(s.abs()));
        for id in ids(Instrument::named().chain([Instrument::Gm(42)]).collect()) {
            let buf = bank.get(id).unwrap();
            assert!(buf.len() > tail, "{id:?} is empty");
            let peak = peak(&buf);
            assert!(peak > 0.0, "{id:?} is silent");
            assert!(peak <= PEAK + 1e-6, "{id:?} peaks at {peak}");
            let end = rms(&buf[buf.len() - tail..]);
            assert!(end < SILENCE, "{id:?} ends at rms {end}");
        }
        // Every key `gm:NN` accepts sounds. Long ones (open hi-hat, long
        // whistle) are cut at 2 s and faded, so their tails are not checked.
        let gm = (crate::model::GM_MIN..=crate::model::GM_MAX).map(Instrument::Gm);
        for id in ids(gm.collect()) {
            let buf = bank.get(id).unwrap();
            let peak = peak(&buf);
            assert!(peak > 0.0 && peak <= PEAK + 1e-6, "{id:?} peaks at {peak}");
        }
    }

    /// Needs FluidSynth and the GM soundfont: `cargo test -- --ignored`.
    #[test]
    #[ignore]
    fn real_soundfont_reports_a_key_with_no_sound() {
        let sf = find_soundfont(None).expect("soundfont");
        let mut bank = Bank::new(Synth::new(48_000, &sf).expect("synth")).expect("bank");
        let id = SoundId {
            instrument: Instrument::Gm(1),
            tone: 'x',
            accent: false,
        };
        let err = format!("{:#}", bank.get(id).unwrap_err());
        assert!(err.contains("has no sound for gm:1 tone x"), "{err}");
        assert!(bank.get(id).is_err(), "nothing cached for a silent key");
    }
}
