//! The audio output stream, the command ring into its callback, and the
//! clock the display extrapolates from.
//!
//! The callback drains commands, starts the user's strokes from the hit
//! ring, publishes the mixer frame it started from together with the
//! `Mono` time it started at, and calls `mix` once. Every timeline the
//! callback lets go of — replaced, dropped from the queue, or swapped out
//! at the end of its segment — leaves through a return ring and is dropped
//! here on the UI thread, and a stroke's buffer is also held by the bank, so the callback allocates
//! nothing, frees nothing and takes no lock.

use std::panic::{self, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering, fence};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

use crate::mixer::{Mixer, mix};
use crate::model::MAX_VOICES;
use crate::timeline::Timeline;

/// Capacity of the command ring and of the hit ring.
const RING: usize = 64;
/// Capacity of the return ring: one command returns at most two timelines
/// (`Load`: the current and the queued one).
const RETURN_RING: usize = 2 * RING;
/// Requested device buffer, in frames: preferred, then the fallback.
const BUFFER_FRAMES: u32 = 128;
const FALLBACK_BUFFER_FRAMES: u32 = 256;

/// A `CLOCK_MONOTONIC` time in nanoseconds: the clock the audio callback
/// and the kernel's input event timestamps share.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Mono(pub u64);

impl Mono {
    pub fn now() -> Mono {
        let mut ts = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        // SAFETY: `ts` is a valid, writable timespec; CLOCK_MONOTONIC is
        // always available on Linux.
        unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
        Mono(ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64)
    }

    /// Signed seconds from `earlier` to `self`.
    pub fn secs_since(self, earlier: Mono) -> f64 {
        (self.0 as i128 - earlier.0 as i128) as f64 * 1e-9
    }
}

impl std::ops::Add<Duration> for Mono {
    type Output = Mono;
    fn add(self, d: Duration) -> Mono {
        Mono(self.0 + d.as_nanos() as u64)
    }
}

impl std::ops::Sub<Duration> for Mono {
    type Output = Mono;
    fn sub(self, d: Duration) -> Mono {
        Mono(self.0 - d.as_nanos() as u64)
    }
}

/// One of the user's strokes, to be sounded by the callback.
pub struct HitSound {
    pub buf: Arc<[f32]>,
    pub gain: f32,
    pub pan: f32,
}

pub enum Command {
    Load(Arc<Timeline>),
    /// Begin `tl` with mutes `muted` where the current timeline ends, if
    /// segment `after` is still the current one.
    Queue {
        tl: Arc<Timeline>,
        after: u32,
        muted: [bool; MAX_VOICES],
    },
    Play,
    Pause,
    Stop,
    /// Mute or unmute `voice` of segment `segment`, if it is the current or
    /// the queued one.
    SetMute {
        voice: usize,
        mute: bool,
        segment: u32,
    },
}

/// One callback's view of the transport.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ClockSnapshot {
    /// Mixer frame at the start of the callback, continuous across the
    /// segments of a score.
    pub frame: u64,
    /// The frame at which the current segment's timeline begins.
    pub origin: u64,
    /// The current segment: 0 for the loaded timeline, one more per swap.
    pub segment: u32,
    pub playing: bool,
    /// When the callback started.
    pub at: Mono,
    /// Time from the callback to the device playing its first frame.
    pub latency: Duration,
    /// Frames the callback was asked for.
    pub period: u32,
    /// Number of commands the callback had applied.
    pub applied: u64,
}

/// Where in the exercise the sound now leaving the speakers is.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Position {
    /// 0-based cycle index.
    pub cycle: u32,
    /// Fraction of the cycle elapsed, in `[0, 1)`.
    pub frac: f64,
}

/// The clock, written by the callback and read by the UI as a seqlock:
/// `seq` is odd while a write is in progress.
struct Clock {
    seq: AtomicU64,
    frame: AtomicU64,
    origin: AtomicU64,
    segment: AtomicU32,
    playing: AtomicBool,
    /// `ClockSnapshot::at`, raw.
    at: AtomicU64,
    latency_nanos: AtomicU64,
    period: AtomicU32,
    applied: AtomicU64,
}

impl Clock {
    fn new() -> Clock {
        Clock {
            seq: AtomicU64::new(0),
            frame: AtomicU64::new(0),
            origin: AtomicU64::new(0),
            segment: AtomicU32::new(0),
            playing: AtomicBool::new(false),
            at: AtomicU64::new(0),
            latency_nanos: AtomicU64::new(0),
            period: AtomicU32::new(0),
            applied: AtomicU64::new(0),
        }
    }

    /// Single writer: the audio callback.
    fn publish(&self, s: &ClockSnapshot) {
        let seq = self.seq.load(Ordering::Relaxed);
        self.seq.store(seq.wrapping_add(1), Ordering::Relaxed);
        fence(Ordering::Release);
        self.frame.store(s.frame, Ordering::Relaxed);
        self.origin.store(s.origin, Ordering::Relaxed);
        self.segment.store(s.segment, Ordering::Relaxed);
        self.playing.store(s.playing, Ordering::Relaxed);
        self.at.store(s.at.0, Ordering::Relaxed);
        self.latency_nanos
            .store(s.latency.as_nanos() as u64, Ordering::Relaxed);
        self.period.store(s.period, Ordering::Relaxed);
        self.applied.store(s.applied, Ordering::Relaxed);
        self.seq.store(seq.wrapping_add(2), Ordering::Release);
    }

    fn read(&self) -> ClockSnapshot {
        loop {
            let before = self.seq.load(Ordering::Acquire);
            if before % 2 == 1 {
                std::hint::spin_loop();
                continue;
            }
            let snap = ClockSnapshot {
                frame: self.frame.load(Ordering::Relaxed),
                origin: self.origin.load(Ordering::Relaxed),
                segment: self.segment.load(Ordering::Relaxed),
                playing: self.playing.load(Ordering::Relaxed),
                at: Mono(self.at.load(Ordering::Relaxed)),
                latency: Duration::from_nanos(self.latency_nanos.load(Ordering::Relaxed)),
                period: self.period.load(Ordering::Relaxed),
                applied: self.applied.load(Ordering::Relaxed),
            };
            fence(Ordering::Acquire);
            if self.seq.load(Ordering::Relaxed) == before {
                return snap;
            }
        }
    }
}

/// The audio callback's state.
pub(crate) struct Callback {
    mixer: Mixer,
    commands: rtrb::Consumer<Command>,
    returned: rtrb::Producer<Arc<Timeline>>,
    hits: rtrb::Consumer<HitSound>,
    clock: Arc<Clock>,
    failure: Arc<OnceLock<String>>,
    applied: u64,
}

impl Callback {
    /// One device callback starting at `now`, `latency` ahead of the
    /// speakers. A panic in `run` is caught here: it records the failure,
    /// and this and every later callback output silence.
    fn process(&mut self, out: &mut [f32], now: Mono, latency: Duration) {
        if self.failure.get().is_some() {
            out.fill(0.0);
            return;
        }
        let ran = panic::catch_unwind(AssertUnwindSafe(|| self.run(out, now, latency)));
        if ran.is_err() {
            let _ = self.failure.set("the audio callback panicked".into());
            out.fill(0.0);
        }
    }

    fn run(&mut self, out: &mut [f32], now: Mono, latency: Duration) {
        // The UI drains the return ring before every push to the command
        // ring and in every frame, the command ring holds `RING` commands,
        // each returning at most two timelines, and a callback swaps at most
        // once per segment it reaches: every push below has room.
        let returned = &mut self.returned;
        let mut retire = |tl: Arc<Timeline>| {
            let _ = returned.push(tl);
        };
        while let Ok(cmd) = self.commands.pop() {
            self.applied += 1;
            match cmd {
                Command::Load(tl) => self
                    .mixer
                    .load(tl)
                    .into_iter()
                    .flatten()
                    .for_each(&mut retire),
                Command::Queue { tl, after, muted } => self
                    .mixer
                    .queue(tl, after, muted)
                    .into_iter()
                    .for_each(&mut retire),
                Command::Play => self.mixer.play(),
                Command::Pause => self.mixer.pause(),
                Command::Stop => self.mixer.stop().into_iter().for_each(&mut retire),
                Command::SetMute {
                    voice,
                    mute,
                    segment,
                } => self.mixer.set_mute(voice, mute, segment),
            }
        }
        while let Ok(h) = self.hits.pop() {
            self.mixer.strike(&h.buf, h.gain, h.pan);
        }
        self.clock.publish(&ClockSnapshot {
            frame: self.mixer.frame,
            origin: self.mixer.origin,
            segment: self.mixer.segment,
            playing: self.mixer.playing,
            at: now,
            latency,
            period: (out.len() / 2) as u32,
            applied: self.applied,
        });
        mix(&mut self.mixer, out, &mut retire);
    }
}

/// The UI side of the audio thread. `S` keeps the output stream alive; the
/// tests use `()` and drive the `Callback` by hand.
pub struct Engine<S = cpal::Stream> {
    _stream: S,
    rate: u32,
    commands: rtrb::Producer<Command>,
    returned: rtrb::Consumer<Arc<Timeline>>,
    /// The producer end of the hit ring until `take_hit_producer`.
    hits: Option<rtrb::Producer<HitSound>>,
    clock: Arc<Clock>,
    /// Set once the stream has died: a stream error or a callback panic.
    failure: Arc<OnceLock<String>>,
    /// Commands sent so far.
    sent: u64,
    /// The value of `sent` once the latest `Load` was sent.
    loaded_at: u64,
}

/// The UI ends of a fresh ring/clock pair, to be joined with a stream.
struct Wiring {
    commands: rtrb::Producer<Command>,
    returned: rtrb::Consumer<Arc<Timeline>>,
    hits: rtrb::Producer<HitSound>,
    clock: Arc<Clock>,
    failure: Arc<OnceLock<String>>,
}

fn wire() -> (Wiring, Callback) {
    let clock = Arc::new(Clock::new());
    let failure = Arc::new(OnceLock::new());
    let (commands, consumer) = rtrb::RingBuffer::new(RING);
    let (producer, returned) = rtrb::RingBuffer::new(RETURN_RING);
    let (hits, hits_consumer) = rtrb::RingBuffer::new(RING);
    let callback = Callback {
        mixer: Mixer::new(),
        commands: consumer,
        returned: producer,
        hits: hits_consumer,
        clock: clock.clone(),
        failure: failure.clone(),
        applied: 0,
    };
    let wiring = Wiring {
        commands,
        returned,
        hits,
        clock,
        failure,
    };
    (wiring, callback)
}

/// The buffer sizes a stream is opened with, in order of preference.
const BUFFER_SIZES: [cpal::BufferSize; 3] = [
    cpal::BufferSize::Fixed(BUFFER_FRAMES),
    cpal::BufferSize::Fixed(FALLBACK_BUFFER_FRAMES),
    cpal::BufferSize::Default,
];

/// The first of `BUFFER_SIZES` that `open` succeeds with; the last error
/// when none does.
fn first_opened<T>(mut open: impl FnMut(cpal::BufferSize) -> Result<T>) -> Result<T> {
    let mut last = None;
    for size in BUFFER_SIZES {
        match open(size) {
            Ok(t) => return Ok(t),
            Err(e) => last = Some(e),
        }
    }
    Err(last.expect("BUFFER_SIZES is not empty"))
}

impl Engine {
    /// Opens the default output device: stereo f32 at its default rate,
    /// 128-frame buffers if the device accepts them, else 256, else its
    /// default size.
    pub fn new() -> Result<Engine> {
        let hint = "check that an audio server (PipeWire or PulseAudio) is running and an output device is available";
        let host = cpal::default_host();
        let device = host
            .default_output_device()
            .ok_or_else(|| anyhow!("no default audio output device; {hint}"))?;
        let rate = device
            .default_output_config()
            .with_context(|| format!("querying the audio output device; {hint}"))?
            .sample_rate();
        let engine = first_opened(|size| Engine::open(&device, rate, size))
            .with_context(|| format!("opening the audio output stream; {hint}"))?;
        engine
            ._stream
            .play()
            .with_context(|| format!("starting the audio output stream; {hint}"))?;
        Ok(engine)
    }

    fn open(device: &cpal::Device, rate: u32, buffer_size: cpal::BufferSize) -> Result<Engine> {
        let (wiring, mut callback) = wire();
        let failure = wiring.failure.clone();
        let config = cpal::StreamConfig {
            channels: 2,
            sample_rate: rate,
            buffer_size,
        };
        let stream = device.build_output_stream(
            config,
            move |out: &mut [f32], info: &cpal::OutputCallbackInfo| {
                let ts = info.timestamp();
                let latency = ts
                    .playback
                    .checked_duration_since(ts.callback)
                    .unwrap_or_default();
                callback.process(out, Mono::now(), latency);
            },
            move |e: cpal::Error| stream_error(&failure, &e),
            None,
        )?;
        Ok(Engine::assemble(stream, rate, wiring))
    }
}

/// Reports a stream error on stderr. Marks the stream dead unless `e` is one
/// cpal recovers from by itself: an xrun (the device is re-prepared), a
/// refused real-time priority, or a reroute to a new default device. The
/// data callback keeps running after those.
fn stream_error(failure: &OnceLock<String>, e: &cpal::Error) {
    eprintln!("djembot: audio stream error: {e}");
    let recovered = matches!(
        e.kind(),
        cpal::ErrorKind::Xrun | cpal::ErrorKind::RealtimeDenied | cpal::ErrorKind::DeviceChanged
    );
    if !recovered {
        let _ = failure.set(format!("audio stream error: {e}"));
    }
}

impl<S> Engine<S> {
    fn assemble(stream: S, rate: u32, w: Wiring) -> Engine<S> {
        Engine {
            _stream: stream,
            rate,
            commands: w.commands,
            returned: w.returned,
            hits: Some(w.hits),
            clock: w.clock,
            failure: w.failure,
            sent: 0,
            loaded_at: 0,
        }
    }

    pub fn sample_rate(&self) -> u32 {
        self.rate
    }

    /// The producer end of the ring of strokes into the callback; `None`
    /// once taken. Its single producer is the input hub.
    pub fn take_hit_producer(&mut self) -> Option<rtrb::Producer<HitSound>> {
        self.hits.take()
    }

    /// Why the stream stopped, once it has; the engine then plays nothing.
    pub fn failure(&self) -> Option<&str> {
        self.failure.get().map(String::as_str)
    }

    pub fn load(&mut self, tl: Arc<Timeline>) -> Result<()> {
        self.send(Command::Load(tl))?;
        self.loaded_at = self.sent;
        Ok(())
    }

    /// Queues `tl` to begin, with mutes `muted`, where segment `after` ends;
    /// the callback ignores it if `after` is no longer the current segment.
    pub fn queue(
        &mut self,
        tl: Arc<Timeline>,
        after: u32,
        muted: [bool; MAX_VOICES],
    ) -> Result<()> {
        self.send(Command::Queue { tl, after, muted })
    }

    pub fn play(&mut self) -> Result<()> {
        self.send(Command::Play)
    }

    pub fn pause(&mut self) -> Result<()> {
        self.send(Command::Pause)
    }

    pub fn stop(&mut self) -> Result<()> {
        self.send(Command::Stop)
    }

    /// Mutes or unmutes `voice` of segment `segment`.
    pub fn set_mute(&mut self, voice: usize, mute: bool, segment: u32) -> Result<()> {
        self.send(Command::SetMute {
            voice,
            mute,
            segment,
        })
    }

    pub fn clock(&self) -> ClockSnapshot {
        self.clock.read()
    }

    /// The frames the device last asked the callback for and the output
    /// latency it reported; `None` before the first callback.
    pub fn audio_status(&self) -> Option<(u32, Duration)> {
        let snap = self.clock();
        (snap.period > 0).then_some((snap.period, snap.latency))
    }

    /// The clock once the callback has applied the latest `Load`; `None`
    /// before then, when it still describes the previous timeline.
    pub fn current(&self) -> Option<ClockSnapshot> {
        Some(self.clock()).filter(|snap| snap.applied >= self.loaded_at)
    }

    /// The number of commands sent so far: a mark `since` compares with.
    pub fn mark(&self) -> u64 {
        self.sent
    }

    /// The clock once the callback has applied every command sent before
    /// `mark` was taken; `None` before then.
    pub fn since(&self, mark: u64) -> Option<ClockSnapshot> {
        Some(self.clock()).filter(|snap| snap.applied >= mark)
    }

    /// Whether a sent command is still waiting for the callback, so the
    /// clock does not yet reflect it. Always false once the stream has
    /// died, since the callback then applies nothing.
    pub fn commands_pending(&self) -> bool {
        self.failure().is_none() && self.clock().applied < self.sent
    }

    /// Whether the score is paused, `tl` beginning at `origin` being the
    /// segment it stopped in: stopped by `pause` somewhere after its start
    /// and before its end. `None` before the callback has applied the latest
    /// `Load`.
    pub fn paused_now(&self, tl: &Timeline, origin: u64) -> Option<bool> {
        let snap = self.current()?;
        Some(paused(&snap, tl, origin))
    }

    /// The position in `tl`, which begins at `origin`, heard at `now`;
    /// `None` while not playing, outside its cycles, or before the callback
    /// has applied the latest `Load`.
    pub fn position_at(&self, tl: &Timeline, origin: u64, now: Mono) -> Option<Position> {
        let snap = self.current()?;
        position(&snap, tl, origin, self.rate, now)
    }

    /// Count-in beat of `tl`, which begins at `origin`, heard at `now` (see
    /// `count_in_beat`); `None` outside the count-in or before the callback
    /// has applied the latest `Load`.
    pub fn count_in_beat_at(&self, tl: &Timeline, origin: u64, now: Mono) -> Option<u32> {
        let snap = self.current()?;
        count_in_beat(&snap, tl, origin, self.rate, now)
    }

    /// Drops the timelines the callback has let go of.
    pub fn drain_returned(&mut self) {
        while self.returned.pop().is_ok() {}
    }

    /// Fails, as a send would, unless the next `n` commands can be sent:
    /// the stream has died or the ring has fewer than `n` free slots.
    pub fn reserve(&mut self, n: usize) -> Result<()> {
        if n == 0 {
            return Ok(());
        }
        if let Some(f) = self.failure() {
            bail!("{f}; audio has stopped, restart `djembot gui`");
        }
        if self.commands.slots() < n {
            bail!("the audio command queue is full; command dropped");
        }
        Ok(())
    }

    /// Drops timelines the callback has replaced, then pushes `cmd`. Fails
    /// if the stream has died or the ring is full.
    fn send(&mut self, cmd: Command) -> Result<()> {
        self.drain_returned();
        self.reserve(1)?;
        if self.commands.push(cmd).is_err() {
            bail!("the audio command queue is full; command dropped");
        }
        self.sent += 1;
        Ok(())
    }
}

/// The mixer frame heard at `t`, extrapolated from `snap`; a `t` before the
/// snapshot extrapolates backwards.
pub fn heard(snap: &ClockSnapshot, rate: u32, t: Mono) -> f64 {
    let ahead = t.secs_since(snap.at);
    snap.frame as f64 + (ahead - snap.latency.as_secs_f64()) * rate as f64
}

/// The position in `tl`, which begins at frame `origin`, heard at `now`,
/// extrapolated from `snap`.
pub fn position(
    snap: &ClockSnapshot,
    tl: &Timeline,
    origin: u64,
    rate: u32,
    now: Mono,
) -> Option<Position> {
    if !snap.playing || tl.cycle_len == 0 {
        return None;
    }
    let heard = heard(snap, rate, now) - origin as f64;
    if let Some(end) = tl.end
        && heard >= end as f64
    {
        return None;
    }
    let into = heard - tl.count_in_len as f64;
    if into < 0.0 {
        return None;
    }
    let cycles = into / tl.cycle_len as f64;
    let cycle = cycles.floor();
    Some(Position {
        cycle: cycle as u32,
        frac: cycles - cycle,
    })
}

/// Whether `snap` shows `tl`, beginning at `origin`, paused rather than
/// stopped: not playing, and neither at the start nor at the end.
pub fn paused(snap: &ClockSnapshot, tl: &Timeline, origin: u64) -> bool {
    !snap.playing && snap.frame != 0 && !finished(snap, origin, tl.end)
}

/// Whether `snap` shows a finite block that begins at `origin` and whose
/// last frame is `end` played to its end: not playing, and at `origin +
/// end`.
pub fn finished(snap: &ClockSnapshot, origin: u64, end: Option<u64>) -> bool {
    !snap.playing && end.is_some_and(|e| snap.frame == origin + e)
}

/// The 1-based count-in beat of `tl`, which begins at `origin`, being heard
/// at `now`: 1 on the first click of an `n`-beat count-in, `n` on the last.
/// `None` when not playing or outside the count-in.
pub fn count_in_beat(
    snap: &ClockSnapshot,
    tl: &Timeline,
    origin: u64,
    rate: u32,
    now: Mono,
) -> Option<u32> {
    let beats = tl.count_in.len() as u64;
    if !snap.playing || beats == 0 || tl.count_in_len == 0 {
        return None;
    }
    let heard = (heard(snap, rate, now) - origin as f64).max(0.0);
    if heard >= tl.count_in_len as f64 {
        return None;
    }
    let beat = (heard * beats as f64 / tl.count_in_len as f64).floor() as u64;
    Some((beat.min(beats - 1) + 1) as u32)
}

#[cfg(test)]
impl Engine<()> {
    /// An engine with no stream, and the callback that stands in for it.
    pub(crate) fn unplugged(rate: u32) -> (Engine<()>, Callback) {
        let (wiring, callback) = wire();
        (Engine::assemble((), rate, wiring), callback)
    }

    /// Marks the stream dead, as a fatal stream error would.
    pub(crate) fn kill(&self, why: &str) {
        let _ = self.failure.set(why.into());
    }
}

#[cfg(test)]
impl Callback {
    /// The mixer's state, as the callback holds it.
    pub(crate) fn mixer(&self) -> &Mixer {
        &self.mixer
    }

    /// One 256-frame callback, as the device would run it now.
    pub(crate) fn tick(&mut self) -> Vec<f32> {
        let mut out = vec![1.0; 512];
        self.process(&mut out, Mono::now(), Duration::ZERO);
        out
    }

    /// One 256-frame callback starting at `now`, `latency` ahead of the
    /// speakers.
    pub(crate) fn tick_at(&mut self, now: Mono, latency: Duration) {
        self.tick_frames(now, latency, 256);
    }

    /// One callback of `frames` frames starting at `now`, `latency` ahead
    /// of the speakers.
    pub(crate) fn tick_frames(&mut self, now: Mono, latency: Duration, frames: usize) {
        let mut out = vec![0.0; frames * 2];
        self.process(&mut out, now, latency);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::MAX_VOICES;

    const RATE: u32 = 48_000;

    fn timeline(count_in_len: u64, cycle_len: u64, cycles: Option<u32>) -> Timeline {
        Timeline {
            cycle_len,
            events: Vec::new(),
            count_in: Vec::new(),
            count_in_len,
            cycles,
            mute: [false; MAX_VOICES],
            end: cycles.map(|c| count_in_len + c as u64 * cycle_len),
            closing: Vec::new(),
        }
    }

    fn snap(frame: u64, playing: bool, at: Mono, latency_ms: u64) -> ClockSnapshot {
        ClockSnapshot {
            frame,
            origin: 0,
            segment: 0,
            playing,
            at,
            latency: Duration::from_millis(latency_ms),
            period: 256,
            applied: 1,
        }
    }

    fn close(p: Option<Position>, cycle: u32, frac: f64) {
        let p = p.unwrap_or_else(|| panic!("expected cycle {cycle} frac {frac}, got None"));
        assert_eq!(p.cycle, cycle, "{p:?}");
        assert!((p.frac - frac).abs() < 1e-9, "{p:?}");
    }

    #[test]
    fn extrapolates_from_the_snapshot() {
        let tl = timeline(96_000, 128_000, None);
        let t0 = Mono::now();
        // At the snapshot instant, with no latency: frame itself.
        close(
            position(&snap(96_000 + 32_000, true, t0, 0), &tl, 0, RATE, t0),
            0,
            0.25,
        );
        // 1 s later is 48000 frames on.
        let s = snap(96_000 + 32_000, true, t0, 0);
        close(
            position(&s, &tl, 0, RATE, t0 + Duration::from_secs(1)),
            0,
            0.625,
        );
        // Wraps into the next cycle.
        close(
            position(&s, &tl, 0, RATE, t0 + Duration::from_secs(2)),
            1,
            0.0,
        );
        let s = snap(96_000 + 5 * 128_000 + 64_000, true, t0, 0);
        close(position(&s, &tl, 0, RATE, t0), 5, 0.5);
    }

    #[test]
    fn subtracts_output_latency() {
        let tl = timeline(0, 48_000, None);
        let t0 = Mono::now();
        // 250 ms after the callback, with 100 ms latency, 150 ms are heard.
        let s = snap(0, true, t0, 100);
        close(
            position(&s, &tl, 0, RATE, t0 + Duration::from_millis(250)),
            0,
            0.15,
        );
    }

    #[test]
    fn none_while_not_playing() {
        let tl = timeline(0, 48_000, None);
        let t0 = Mono::now();
        assert_eq!(position(&snap(1000, false, t0, 0), &tl, 0, RATE, t0), None);
    }

    #[test]
    fn none_during_count_in() {
        let tl = timeline(96_000, 128_000, Some(2));
        let t0 = Mono::now();
        let s = snap(0, true, t0, 0);
        assert_eq!(position(&s, &tl, 0, RATE, t0), None);
        // 1.99 s in is still 960 frames short of the end of the count-in.
        assert_eq!(
            position(&s, &tl, 0, RATE, t0 + Duration::from_millis(1980)),
            None
        );
        close(
            position(&s, &tl, 0, RATE, t0 + Duration::from_secs(2)),
            0,
            0.0,
        );
        // Latency delays the end of the count-in as heard.
        let s = snap(96_000, true, t0, 10);
        assert_eq!(position(&s, &tl, 0, RATE, t0), None);
    }

    #[test]
    fn none_past_end() {
        let tl = timeline(96_000, 128_000, Some(2));
        let end = tl.end.unwrap();
        let t0 = Mono::now();
        close(
            position(&snap(end - 1, true, t0, 0), &tl, 0, RATE, t0),
            1,
            127_999.0 / 128_000.0,
        );
        assert_eq!(position(&snap(end, true, t0, 0), &tl, 0, RATE, t0), None);
        let s = snap(end - 48_000, true, t0, 0);
        assert_eq!(
            position(&s, &tl, 0, RATE, t0 + Duration::from_secs(1)),
            None
        );
        // Looping exercises never end.
        let tl = timeline(96_000, 128_000, None);
        assert!(position(&snap(end, true, t0, 0), &tl, 0, RATE, t0).is_some());
    }

    #[test]
    fn clock_round_trips_a_snapshot() {
        let c = Clock::new();
        let s = ClockSnapshot {
            frame: 123_456,
            origin: 100_000,
            segment: 3,
            playing: true,
            at: Mono(987_654_321),
            latency: Duration::from_micros(21_333),
            period: 128,
            applied: 7,
        };
        c.publish(&s);
        assert_eq!(c.read(), s);
        assert_eq!(c.seq.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn audio_status_is_the_latest_period_and_latency() {
        let (engine, mut cb) = Engine::<()>::unplugged(RATE);
        assert_eq!(engine.audio_status(), None);
        cb.tick_frames(Mono(1_000), Duration::from_millis(9), 128);
        assert_eq!(engine.audio_status(), Some((128, Duration::from_millis(9))));
        cb.tick_at(Mono(2_000), Duration::from_millis(17));
        assert_eq!(
            engine.audio_status(),
            Some((256, Duration::from_millis(17)))
        );
    }

    #[test]
    fn clock_reads_are_never_torn() {
        let c = Arc::new(Clock::new());
        let writer = {
            let c = c.clone();
            std::thread::spawn(move || {
                for i in 0..200_000u64 {
                    c.publish(&ClockSnapshot {
                        frame: i,
                        origin: i,
                        segment: i as u32,
                        playing: i % 2 == 1,
                        at: Mono(i),
                        latency: Duration::from_nanos(i),
                        period: i as u32,
                        applied: i,
                    });
                }
            })
        };
        while !writer.is_finished() {
            let s = c.read();
            assert_eq!(s.applied, s.frame);
            assert_eq!(s.origin, s.frame);
            assert_eq!(s.segment, s.frame as u32);
            assert_eq!(s.playing, s.frame % 2 == 1);
            assert_eq!(s.latency.as_nanos() as u64, s.frame);
            assert_eq!(s.period, s.frame as u32);
            assert_eq!(s.at.0, s.frame);
        }
        writer.join().unwrap();
    }

    #[test]
    fn heard_extrapolates_backwards_before_the_snapshot() {
        let t0 = Mono(10_000_000_000);
        let s = snap(96_000, true, t0, 0);
        assert_eq!(heard(&s, RATE, t0), 96_000.0);
        assert_eq!(heard(&s, RATE, t0 - Duration::from_millis(500)), 72_000.0);
        assert_eq!(heard(&s, RATE, t0 + Duration::from_millis(500)), 120_000.0);
        // Latency counts the same way on either side.
        let s = snap(96_000, true, t0, 100);
        assert_eq!(heard(&s, RATE, t0 - Duration::from_millis(400)), 72_000.0);
    }

    #[test]
    fn mono_now_is_monotonic_and_signed_differences_work() {
        let a = Mono::now();
        let b = Mono::now();
        assert!(b >= a);
        assert_eq!(Mono(3_000_000_000).secs_since(Mono(1_000_000_000)), 2.0);
        assert_eq!(Mono(1_000_000_000).secs_since(Mono(3_000_000_000)), -2.0);
    }

    fn unplugged() -> (Engine<()>, Callback) {
        Engine::unplugged(RATE)
    }

    fn sound(v: f32) -> HitSound {
        HitSound {
            buf: Arc::from(vec![v; 4]),
            gain: 1.0,
            pan: 0.0,
        }
    }

    #[test]
    fn a_struck_sound_starts_at_frame_0_of_the_next_callback() {
        let (mut e, mut cb) = unplugged();
        let mut hits = e.take_hit_producer().expect("not taken yet");
        assert!(e.take_hit_producer().is_none(), "taken once");
        assert!(tick(&mut cb).iter().all(|s| *s == 0.0), "silence before");
        hits.push(sound(1.0)).unwrap();
        let out = tick(&mut cb);
        let centre = std::f32::consts::FRAC_1_SQRT_2;
        for f in 0..4 {
            assert!(
                (out[2 * f] - centre).abs() < 1e-6,
                "frame {f}: {}",
                out[2 * f]
            );
            assert!((out[2 * f + 1] - centre).abs() < 1e-6);
        }
        assert!(out[8..].iter().all(|s| *s == 0.0));
        // Stopped transport: strokes sound whatever the mixer is doing.
        assert!(!cb.mixer.playing);
    }

    #[test]
    fn a_full_hit_ring_drops_the_stroke_without_blocking() {
        let (mut e, mut cb) = unplugged();
        let mut hits = e.take_hit_producer().unwrap();
        for _ in 0..RING {
            hits.push(sound(0.0)).unwrap();
        }
        assert!(hits.push(sound(0.0)).is_err());
        tick(&mut cb);
        hits.push(sound(0.0)).unwrap();
    }

    fn tick(cb: &mut Callback) -> Vec<f32> {
        cb.tick()
    }

    #[test]
    fn position_waits_for_the_callback_to_apply_each_load() {
        let (mut e, mut cb) = unplugged();
        let first = Arc::new(timeline(0, 48_000, None));
        e.load(first.clone()).unwrap();
        e.play().unwrap();
        assert_eq!(
            e.position_at(&first, 0, Mono::now()),
            None,
            "Load not yet applied"
        );
        tick(&mut cb);
        assert_eq!(cb.applied, 2);
        let p = e
            .position_at(&first, 0, Mono::now())
            .expect("Load applied, playing");
        assert_eq!(p.cycle, 0);

        let second = Arc::new(timeline(0, 96_000, None));
        e.load(second.clone()).unwrap();
        e.play().unwrap();
        assert_eq!(
            e.position_at(&second, 0, Mono::now()),
            None,
            "second Load not yet applied"
        );
        tick(&mut cb);
        assert_eq!(cb.applied, 4);
        assert!(e.position_at(&second, 0, Mono::now()).is_some());
        // The callback handed the replaced timeline back; the next send
        // drops it on this thread.
        assert_eq!(Arc::strong_count(&first), 2);
        e.stop().unwrap();
        assert_eq!(Arc::strong_count(&first), 1);
    }

    #[test]
    fn a_dead_stream_fails_every_send() {
        let (mut e, _cb) = unplugged();
        assert_eq!(e.failure(), None);
        e.failure
            .set("audio stream error: device gone".into())
            .unwrap();
        let err = e.play().unwrap_err().to_string();
        assert_eq!(
            err,
            "audio stream error: device gone; audio has stopped, restart `djembot gui`"
        );
        assert!(e.load(Arc::new(timeline(0, 48_000, None))).is_err());
        assert_eq!(e.sent, 0);
    }

    #[test]
    fn a_full_ring_fails_the_send() {
        let (mut e, _cb) = unplugged();
        e.reserve(RING).unwrap();
        for _ in 0..RING - 1 {
            e.play().unwrap();
        }
        e.reserve(1).unwrap();
        assert!(e.reserve(2).is_err());
        e.play().unwrap();
        let err = e.play().unwrap_err().to_string();
        assert!(err.contains("queue is full"), "{err}");
        assert!(e.reserve(1).is_err());
        e.reserve(0).unwrap();
    }

    #[test]
    fn only_unrecovered_stream_errors_kill_the_stream() {
        let failure = OnceLock::new();
        for kind in [
            cpal::ErrorKind::Xrun,
            cpal::ErrorKind::RealtimeDenied,
            cpal::ErrorKind::DeviceChanged,
        ] {
            stream_error(&failure, &kind.into());
        }
        assert_eq!(failure.get(), None);
        stream_error(&failure, &cpal::ErrorKind::DeviceNotAvailable.into());
        let f = failure.get().unwrap();
        assert!(f.starts_with("audio stream error: "), "{f}");

        let (mut e, _cb) = unplugged();
        stream_error(&e.failure, &cpal::ErrorKind::Xrun.into());
        e.play().unwrap();
        e.reserve(0).unwrap();
        e.kill("gone");
        assert!(e.reserve(1).is_err());
    }

    /// Relies on debug-build overflow checks for the panic.
    #[cfg(debug_assertions)]
    #[test]
    fn a_callback_panic_is_recorded_and_silenced() {
        let (mut e, mut cb) = unplugged();
        // Count-in events out of order make `mix` subtract past zero.
        let click = |at| crate::timeline::Event {
            at,
            buf: Arc::from(vec![1.0f32; 4]),
            gain: 1.0,
            pan: 0.0,
            voice: MAX_VOICES,
            pulse: 0,
        };
        let mut tl = timeline(200, 48_000, None);
        tl.count_in = vec![click(100), click(50)];
        e.load(Arc::new(tl)).unwrap();
        e.play().unwrap();
        assert!(tick(&mut cb).iter().all(|s| *s == 0.0));
        assert_eq!(e.failure(), Some("the audio callback panicked"));
        assert!(tick(&mut cb).iter().all(|s| *s == 0.0));
        assert!(e.play().is_err());
    }

    #[test]
    fn count_in_beat_counts_up() {
        let mut tl = timeline(96_000, 128_000, Some(2));
        let click = |at| crate::timeline::Event {
            at,
            buf: Arc::from(vec![1.0f32]),
            gain: 1.0,
            pan: 0.0,
            voice: MAX_VOICES,
            pulse: 0,
        };
        tl.count_in = (0..4).map(|b| click(b * 24_000)).collect();
        let t0 = Mono::now();
        let at = |frame| count_in_beat(&snap(frame, true, t0, 0), &tl, 0, RATE, t0);
        assert_eq!(at(0), Some(1));
        assert_eq!(at(23_999), Some(1));
        assert_eq!(at(24_000), Some(2));
        assert_eq!(at(95_999), Some(4));
        assert_eq!(at(96_000), None, "cycle 0 has started");
        // Latency puts the heard frame before 0 at the very start.
        assert_eq!(
            count_in_beat(&snap(0, true, t0, 20), &tl, 0, RATE, t0),
            Some(1)
        );
        assert_eq!(
            count_in_beat(&snap(0, false, t0, 0), &tl, 0, RATE, t0),
            None
        );
        tl.count_in.clear();
        assert_eq!(count_in_beat(&snap(0, true, t0, 0), &tl, 0, RATE, t0), None);
    }

    #[test]
    fn current_waits_for_the_latest_load() {
        let (mut e, mut cb) = unplugged();
        assert!(e.current().is_some(), "no Load sent yet");
        e.load(Arc::new(timeline(0, 48_000, None))).unwrap();
        assert_eq!(e.current(), None);
        tick(&mut cb);
        assert_eq!(e.current().map(|s| s.applied), Some(1));
        // Commands sent after the Load leave `current` alone.
        e.play().unwrap();
        assert!(e.current().is_some());
    }

    #[test]
    fn commands_pending_until_the_callback_applies_them() {
        let (mut e, mut cb) = unplugged();
        assert!(!e.commands_pending());
        e.load(Arc::new(timeline(0, 48_000, None))).unwrap();
        e.play().unwrap();
        assert!(e.commands_pending());
        tick(&mut cb);
        assert!(!e.commands_pending());
        // A bare Play on the loaded timeline is pending until applied too.
        e.stop().unwrap();
        tick(&mut cb);
        e.play().unwrap();
        assert!(e.commands_pending());
        assert!(!e.current().unwrap().playing, "the clock lags the Play");
        tick(&mut cb);
        assert!(!e.commands_pending());
        assert!(e.current().unwrap().playing);
        // A failed send is not pending; nor is anything once the stream died.
        for _ in 0..RING {
            e.set_mute(0, true, 0).unwrap();
        }
        assert!(e.set_mute(0, true, 0).is_err());
        tick(&mut cb);
        assert!(!e.commands_pending());
        e.set_mute(0, false, 0).unwrap();
        e.failure.set("audio stream error: gone".into()).unwrap();
        assert!(!e.commands_pending());
    }

    #[test]
    fn paused_is_neither_at_the_start_nor_at_the_end() {
        let tl = timeline(0, 48_000, Some(2));
        let t0 = Mono::now();
        assert!(!paused(&snap(0, false, t0, 0), &tl, 0), "stopped");
        assert!(paused(&snap(1_000, false, t0, 0), &tl, 0));
        assert!(!paused(&snap(96_000, false, t0, 0), &tl, 0), "finished");
        assert!(!paused(&snap(1_000, true, t0, 0), &tl, 0), "playing");
        let (mut e, mut cb) = unplugged();
        let tl = Arc::new(tl);
        e.load(tl.clone()).unwrap();
        assert_eq!(e.paused_now(&tl, 0), None, "Load not yet applied");
        e.play().unwrap();
        tick(&mut cb);
        e.pause().unwrap();
        tick(&mut cb);
        assert_eq!(e.paused_now(&tl, 0), Some(true));
        e.stop().unwrap();
        tick(&mut cb);
        assert_eq!(e.paused_now(&tl, 0), Some(false));
    }

    /// A finite timeline of `cycles` 256-frame cycles, no count-in.
    fn short(cycles: u32) -> Arc<Timeline> {
        Arc::new(timeline(0, 256, Some(cycles)))
    }

    #[test]
    fn every_displaced_timeline_is_returned() {
        let (mut e, mut cb) = unplugged();
        let none = [false; MAX_VOICES];
        // Load over a current and a queued timeline returns both.
        let (a, b) = (short(4), short(4));
        e.load(a.clone()).unwrap();
        e.queue(b.clone(), 0, none).unwrap();
        tick(&mut cb);
        e.load(short(4)).unwrap();
        tick(&mut cb);
        e.drain_returned();
        assert_eq!((Arc::strong_count(&a), Arc::strong_count(&b)), (1, 1));
        // Stop with a queued timeline returns it.
        let c = short(4);
        e.queue(c.clone(), 0, none).unwrap();
        e.stop().unwrap();
        tick(&mut cb);
        e.drain_returned();
        assert_eq!(Arc::strong_count(&c), 1);
        // A stale Queue returns its payload.
        let d = short(4);
        e.queue(d.clone(), 5, none).unwrap();
        tick(&mut cb);
        e.drain_returned();
        assert_eq!(Arc::strong_count(&d), 1);
        // A Queue over a queued timeline returns the old one.
        let (f, g) = (short(4), short(4));
        e.queue(f.clone(), 0, none).unwrap();
        e.queue(g.clone(), 0, none).unwrap();
        tick(&mut cb);
        e.drain_returned();
        assert_eq!(Arc::strong_count(&f), 1);
        // The timeline swapped out at its end is returned too.
        let (h, i) = (short(1), short(4));
        e.load(h.clone()).unwrap();
        e.queue(i.clone(), 0, none).unwrap();
        e.play().unwrap();
        tick(&mut cb);
        tick(&mut cb);
        assert_eq!(e.clock().segment, 1);
        e.drain_returned();
        assert_eq!((Arc::strong_count(&g), Arc::strong_count(&h)), (1, 1));
        assert_eq!(Arc::strong_count(&i), 2, "the callback still holds it");
    }

    #[test]
    fn the_snapshot_carries_the_segment_and_its_origin() {
        let (mut e, mut cb) = unplugged();
        e.load(short(1)).unwrap();
        e.queue(short(2), 0, [false; MAX_VOICES]).unwrap();
        e.play().unwrap();
        tick(&mut cb);
        let s = e.clock();
        assert_eq!((s.frame, s.origin, s.segment), (0, 0, 0));
        tick(&mut cb);
        let s = e.clock();
        assert_eq!((s.frame, s.origin, s.segment), (256, 256, 1));
        e.set_mute(2, true, 1).unwrap();
        tick(&mut cb);
        assert!(cb.mixer.muted[2]);
    }

    #[test]
    fn position_uses_the_supplied_origin() {
        let tl = timeline(96_000, 128_000, Some(2));
        let t0 = Mono::now();
        let origin = 384_000;
        let s = snap(origin + 96_000 + 32_000, true, t0, 0);
        close(position(&s, &tl, origin, RATE, t0), 0, 0.25);
        // Before the origin, the segment has not begun.
        let s = snap(origin - 1_000, true, t0, 0);
        assert_eq!(position(&s, &tl, origin, RATE, t0), None);
        let mut tl = tl;
        tl.count_in = vec![crate::timeline::Event {
            at: 0,
            buf: Arc::from(vec![1.0f32]),
            gain: 1.0,
            pan: 0.0,
            voice: MAX_VOICES,
            pulse: 0,
        }];
        assert_eq!(
            count_in_beat(&snap(origin + 10, true, t0, 0), &tl, origin, RATE, t0),
            Some(1)
        );
        assert_eq!(
            count_in_beat(&snap(origin + 96_000, true, t0, 0), &tl, origin, RATE, t0),
            None
        );
    }

    #[test]
    fn finished_compares_against_origin_plus_end() {
        let tl = timeline(0, 48_000, Some(2));
        let t0 = Mono::now();
        let end = tl.end;
        assert!(finished(&snap(96_000, false, t0, 0), 0, end));
        assert!(!finished(&snap(96_000, false, t0, 0), 1_000, end));
        assert!(finished(&snap(97_000, false, t0, 0), 1_000, end));
        assert!(!finished(&snap(97_000, true, t0, 0), 1_000, end), "playing");
        assert!(
            !finished(&snap(97_000, false, t0, 0), 1_000, None),
            "looping"
        );
        assert!(!paused(&snap(97_000, false, t0, 0), &tl, 1_000));
        assert!(paused(&snap(96_000, false, t0, 0), &tl, 1_000));
    }

    #[test]
    fn queue_reserves_a_slot() {
        let (mut e, _cb) = unplugged();
        for _ in 0..RING - 1 {
            e.play().unwrap();
        }
        e.queue(short(1), 0, [false; MAX_VOICES]).unwrap();
        let err = e.queue(short(1), 0, [false; MAX_VOICES]).unwrap_err();
        assert!(err.to_string().contains("queue is full"), "{err}");
        assert_eq!(e.mark(), RING as u64);
    }

    #[test]
    fn streams_open_with_128_frames_then_256_then_the_default() {
        use cpal::BufferSize::{Default, Fixed};
        let mut tried = Vec::new();
        let opened = first_opened(|size| {
            tried.push(size);
            if size == Default {
                Ok(size)
            } else {
                Err(anyhow!("refused"))
            }
        });
        assert_eq!(opened.unwrap(), Default);
        assert_eq!(tried, [Fixed(128), Fixed(256), Default]);
        let mut tried = 0;
        let opened = first_opened(|size| {
            tried += 1;
            Ok(size)
        });
        assert_eq!((opened.unwrap(), tried), (Fixed(128), 1));
        let failed: Result<()> = first_opened(|_| Err(anyhow!("no device")));
        assert_eq!(failed.unwrap_err().to_string(), "no device");
    }

    /// Needs an audio output device: `cargo test -- --ignored`.
    #[test]
    #[ignore]
    fn plays_on_the_default_device() {
        use crate::sounds::Bank;
        use crate::timeline::tests::exercise;
        let mut engine = Engine::new().unwrap();
        let rate = engine.sample_rate();
        let mut bank = Bank::from_fn(|_| Arc::from(vec![0.0f32; 16]));
        let ex = exercise(serde_json::json!({
            "tempo": 120, "beats": 4,
            "voices": [{ "name": "c", "instrument": "clave", "pulses": 4, "onsets": [0] }]
        }));
        let tl = Arc::new(Timeline::from(&ex, &mut bank, rate).unwrap());
        engine.load(tl.clone()).unwrap();
        engine.play().unwrap();
        std::thread::sleep(Duration::from_millis(300));
        let snap = engine.clock();
        assert!(snap.playing);
        assert_eq!(snap.applied, 2);
        assert!(snap.frame > 0);
        println!(
            "period {} frames, latency {:.1} ms",
            snap.period,
            snap.latency.as_secs_f64() * 1000.0
        );
        assert!(snap.period > 0 && snap.period <= 256, "{snap:?}");
        assert!(snap.latency < Duration::from_millis(50), "{snap:?}");
        assert!(engine.position_at(&tl, 0, Mono::now()).is_some());
        engine.stop().unwrap();
        std::thread::sleep(Duration::from_millis(100));
        assert!(!engine.clock().playing);
        assert_eq!(engine.position_at(&tl, 0, Mono::now()), None);
    }
}
