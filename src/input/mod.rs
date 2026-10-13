//! Keyboard input: the hub thread that reads strokes from every input source,
//! sounds the mapped ones through the engine and forwards them to the window.
//!
//! A stroke carries the kernel's timestamp unchanged from the source to the
//! window; how quickly the hub wakes affects only when the sample sounds.

pub mod evdev;
pub mod keys;

use std::collections::{HashMap, HashSet};
use std::io;
use std::os::fd::{AsRawFd, BorrowedFd};
use std::os::unix::fs::DirEntryExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use crate::engine::{HitSound, Mono};

/// How often the hub looks for new devices and refreshes its status.
const RESCAN: Duration = Duration::from_secs(2);
/// Where the evdev devices live.
const INPUT_DIR: &str = "/dev/input";

/// A stroke, as the source saw it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Hit {
    /// Kernel timestamp.
    pub at: Mono,
    pub trigger: Trigger,
    /// `None` from a plain keyboard.
    pub velocity: Option<f32>,
}

/// What was struck, independent of which source saw it.
#[derive(Clone, Copy, Debug, Hash, Eq, PartialEq)]
pub enum Trigger {
    /// An evdev key code (`KEY_F` = 33, …).
    Key(u16),
}

/// One device the hub reads strokes from.
pub trait Source: Send {
    fn name(&self) -> &str;
    /// The descriptor the hub polls for readability.
    fn fd(&self) -> BorrowedFd<'_>;
    /// Drains everything readable now; appends one `Hit` per stroke
    /// (presses only — a source decides what a press is). `Err` means the
    /// device is gone and the hub drops the source.
    fn read(&mut self, into: &mut Vec<Hit>) -> io::Result<()>;
}

/// The player parts of the loaded block, by trigger.
#[derive(Clone, Default)]
pub struct Arming {
    pub parts: HashMap<Trigger, Part>,
}

/// The sound one trigger plays: its part's voice at the bound tone —
/// `None` for a silent part.
#[derive(Clone, Debug)]
pub struct Part {
    pub buf: Option<Arc<[f32]>>,
    pub gain: f32,
    pub pan: f32,
}

impl Part {
    /// The stroke's sound, for the engine's hit ring; `None` when the part
    /// is silent.
    pub fn sound(&self) -> Option<HitSound> {
        Some(HitSound {
            buf: self.buf.clone()?,
            gain: self.gain,
            pan: self.pan,
        })
    }
}

/// What the readable devices are, as of the hub's last walk.
#[derive(Clone, Debug, PartialEq)]
pub enum InputStatus {
    /// This many keyboards are open.
    Devices(usize),
    /// No keyboard is open and this device could not be opened for lack of
    /// permission.
    NoAccess(String),
    /// No keyboard is open and none was refused.
    None,
}

impl InputStatus {
    /// `2 devices`, `no access to /dev/input/event3`, `no keyboard found`.
    pub fn summary(&self) -> String {
        match self {
            InputStatus::Devices(1) => "1 device".into(),
            InputStatus::Devices(n) => format!("{n} devices"),
            InputStatus::NoAccess(path) => format!("no access to {path}"),
            InputStatus::None => "no keyboard found".into(),
        }
    }

    /// The status line's text.
    pub fn status_text(&self) -> String {
        match self {
            InputStatus::NoAccess(_) => format!(
                "keys: {} — add yourself to the input group and log in again",
                self.summary()
            ),
            _ => format!("keys: {}", self.summary()),
        }
    }
}

/// State shared between the window and the hub.
pub struct Shared {
    /// Whether the window has keyboard focus and is shown; written by the
    /// window on every pass.
    focused: AtomicBool,
    /// The `Mono` nanoseconds after which `focused` no longer counts: the
    /// window passes only as often as it is redrawn, and a compositor may
    /// stop redrawing a hidden window without a pass that clears `focused`.
    focused_until: AtomicU64,
    /// The loaded block's parts; `None` when it has none.
    pub arming: Mutex<Option<Arc<Arming>>>,
    pub status: Mutex<InputStatus>,
}

impl Default for Shared {
    fn default() -> Self {
        Shared {
            focused: AtomicBool::new(false),
            focused_until: AtomicU64::new(u64::MAX),
            arming: Mutex::new(None),
            status: Mutex::new(InputStatus::None),
        }
    }
}

impl Shared {
    /// Records whether the window has focus, valid until `until`; `None`
    /// when the window is not expecting to pass again until something
    /// happens, so the value holds until the next pass.
    pub fn set_focus(&self, focused: bool, until: Option<Mono>) {
        self.focused.store(focused, Ordering::Relaxed);
        self.focused_until
            .store(until.map_or(u64::MAX, |t| t.0), Ordering::Relaxed);
    }

    /// Whether a stroke at `now` is the user playing the window: it has
    /// focus, as of a pass recent enough to still count.
    pub fn focused_at(&self, now: Mono) -> bool {
        self.focused.load(Ordering::Relaxed) && now.0 <= self.focused_until.load(Ordering::Relaxed)
    }

    pub fn set_arming(&self, arming: Option<Arming>) {
        *lock(&self.arming) = arming.map(Arc::new);
    }

    pub fn arming(&self) -> Option<Arc<Arming>> {
        lock(&self.arming).clone()
    }

    pub fn status(&self) -> InputStatus {
        lock(&self.status).clone()
    }
}

/// Locks `m`, taking a poisoned lock anyway: a panic on one thread is
/// printed by the panic hook and does not stop the threads that share `m`.
pub(crate) fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Locks `m` as `lock` does, waiting no later than `deadline`: `None` if
/// another thread held it throughout.
pub(crate) fn lock_until<T>(m: &Mutex<T>, deadline: Instant) -> Option<MutexGuard<'_, T>> {
    loop {
        match m.try_lock() {
            Ok(guard) => return Some(guard),
            Err(std::sync::TryLockError::Poisoned(e)) => return Some(e.into_inner()),
            Err(std::sync::TryLockError::WouldBlock) => {}
        }
        let now = Instant::now();
        if now >= deadline {
            return None;
        }
        thread::sleep(LOCK_POLL.min(deadline - now));
    }
}

/// How often `lock_until` retries a held lock.
const LOCK_POLL: Duration = Duration::from_millis(2);

/// The part a stroke plays: none while the window is unfocused, when no
/// block is armed, or when the trigger is not mapped.
pub fn route(focused: bool, arming: Option<&Arming>, hit: &Hit) -> Option<Part> {
    if !focused {
        return None;
    }
    arming?.parts.get(&hit.trigger).cloned()
}

/// An open source and the path it was opened from.
struct Open {
    path: PathBuf,
    source: Box<dyn Source>,
}

/// A directory entry under `/dev/input`. The inode tells a node recreated
/// at the same path (another device plugged in) from the one seen before.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct Node {
    path: PathBuf,
    ino: u64,
}

/// Opens every node in `nodes` whose path is not already in `sources` and
/// that is not in `rejected`, with `open` — `Ok(Some)` a keyboard, kept;
/// `Ok(None)` another device, closed and added to `rejected` so later walks
/// skip it; `Err` skipped and retried on the next walk — and returns the
/// status that results. `rejected` is pruned to the nodes listed. A failed
/// open comes before the node can be told to be a keyboard, so only a
/// permission denial, which says why every keyboard is missing, reaches the
/// status.
fn walk(
    nodes: impl IntoIterator<Item = Node>,
    sources: &mut Vec<Open>,
    rejected: &mut HashSet<Node>,
    mut open: impl FnMut(&Path) -> io::Result<Option<Box<dyn Source>>>,
) -> InputStatus {
    let mut denied = None;
    let mut listed = HashSet::new();
    for node in nodes {
        if sources.iter().any(|s| s.path == node.path) || rejected.contains(&node) {
            listed.insert(node);
            continue;
        }
        match open(&node.path) {
            Ok(Some(source)) => sources.push(Open {
                path: node.path.clone(),
                source,
            }),
            Ok(None) => {
                rejected.insert(node.clone());
            }
            Err(e) if e.kind() == io::ErrorKind::PermissionDenied => {
                denied.get_or_insert_with(|| node.path.display().to_string());
            }
            Err(_) => {}
        }
        listed.insert(node);
    }
    rejected.retain(|n| listed.contains(n));
    if !sources.is_empty() {
        InputStatus::Devices(sources.len())
    } else if let Some(path) = denied {
        InputStatus::NoAccess(path)
    } else {
        InputStatus::None
    }
}

/// The `event*` entries of `dir`, sorted; none if it cannot be read.
fn event_nodes(dir: &Path) -> Vec<Node> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut nodes: Vec<Node> = entries
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().starts_with("event"))
        .map(|e| Node {
            path: e.path(),
            ino: e.ino(),
        })
        .collect();
    nodes.sort_by(|a, b| a.path.cmp(&b.path));
    nodes
}

/// Starts the hub thread. Each mapped stroke is pushed into `engine_hits`
/// (dropped when the ring is full) and sent with its part to `to_ui`, then
/// `wake` is called. The thread ends when `to_ui`'s receiver is dropped.
pub fn spawn(
    engine_hits: rtrb::Producer<HitSound>,
    to_ui: mpsc::Sender<(Hit, Part)>,
    wake: impl Fn() + Send + 'static,
    shared: Arc<Shared>,
) -> io::Result<thread::JoinHandle<()>> {
    thread::Builder::new()
        .name("djembot-input".into())
        .spawn(move || hub(engine_hits, to_ui, wake, shared))
}

fn hub(
    mut engine_hits: rtrb::Producer<HitSound>,
    to_ui: mpsc::Sender<(Hit, Part)>,
    wake: impl Fn(),
    shared: Arc<Shared>,
) {
    let mut sources: Vec<Open> = Vec::new();
    let mut rejected: HashSet<Node> = HashSet::new();
    let mut hits = Vec::new();
    let mut walked: Option<Instant> = None;
    loop {
        if walked.is_none_or(|t| t.elapsed() >= RESCAN) {
            let status = walk(
                event_nodes(Path::new(INPUT_DIR)),
                &mut sources,
                &mut rejected,
                |p| Ok(evdev::open(p)?.map(|s| Box::new(s) as Box<dyn Source>)),
            );
            *lock(&shared.status) = status;
            walked = Some(Instant::now());
        }
        let wait = RESCAN.saturating_sub(walked.map_or(Duration::ZERO, |t| t.elapsed()));
        let mut fds: Vec<libc::pollfd> = sources
            .iter()
            .map(|s| libc::pollfd {
                fd: s.source.fd().as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            })
            .collect();
        // SAFETY: `fds` is a valid array of `fds.len()` pollfds, and every
        // descriptor in it is owned by a source that outlives the call.
        let ready = unsafe {
            libc::poll(
                fds.as_mut_ptr(),
                fds.len() as libc::nfds_t,
                wait.as_millis() as libc::c_int,
            )
        };
        if ready <= 0 {
            continue;
        }
        let ready: Vec<bool> = fds.iter().map(|fd| fd.revents != 0).collect();
        read_ready(&mut sources, &ready, &mut hits);
        if dispatch(&mut hits, &mut engine_hits, &to_ui, &shared, Mono::now()) {
            return;
        }
        wake();
    }
}

/// Reads every source whose entry in `ready` is true into `hits`, and drops
/// the sources whose read failed, so the next walk can reopen their nodes.
/// Strokes a failed source read before the error are kept.
fn read_ready(sources: &mut Vec<Open>, ready: &[bool], hits: &mut Vec<Hit>) {
    let mut i = 0;
    sources.retain_mut(|s| {
        let read = ready.get(i).copied().unwrap_or(false);
        i += 1;
        if !read {
            return true;
        }
        match s.source.read(hits) {
            Ok(()) => true,
            Err(e) => {
                eprintln!("djembot: input device {} dropped: {e}", s.source.name());
                false
            }
        }
    });
}

/// Routes and dispatches `hits`, emptying it, as of `now`. True when the
/// window is gone.
fn dispatch(
    hits: &mut Vec<Hit>,
    engine_hits: &mut rtrb::Producer<HitSound>,
    to_ui: &mpsc::Sender<(Hit, Part)>,
    shared: &Shared,
    now: Mono,
) -> bool {
    if hits.is_empty() {
        return false;
    }
    let focused = shared.focused_at(now);
    let arming = shared.arming();
    for hit in hits.drain(..) {
        let Some(part) = route(focused, arming.as_deref(), &hit) else {
            continue;
        };
        if let Some(sound) = part.sound() {
            let _ = engine_hits.push(sound);
        }
        if to_ui.send((hit, part)).is_err() {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lock_until_gives_up_at_the_deadline() {
        let m = Mutex::new(1);
        assert_eq!(lock_until(&m, Instant::now()).map(|g| *g), Some(1));
        let held = m.lock().unwrap();
        let start = Instant::now();
        assert!(lock_until(&m, start + Duration::from_millis(30)).is_none());
        assert!(start.elapsed() >= Duration::from_millis(30));
        drop(held);
        let m = Arc::new(m);
        let holder = m.clone();
        let _ = thread::spawn(move || {
            let _g = holder.lock().unwrap();
            panic!("poison");
        })
        .join();
        assert!(m.is_poisoned());
        assert_eq!(lock_until(&m, Instant::now()).map(|g| *g), Some(1));
    }

    fn part() -> Part {
        Part {
            buf: Some(Arc::from(vec![0.5f32; 4])),
            gain: 0.7,
            pan: -0.4,
        }
    }

    fn hit(code: u16) -> Hit {
        Hit {
            at: Mono(1_000),
            trigger: Trigger::Key(code),
            velocity: None,
        }
    }

    fn armed() -> Arming {
        Arming {
            parts: HashMap::from([(Trigger::Key(33), part())]),
        }
    }

    #[test]
    fn route_drops_unfocused_unarmed_and_unmapped_strokes() {
        let a = armed();
        assert!(route(false, Some(&a), &hit(33)).is_none(), "unfocused");
        assert!(route(true, None, &hit(33)).is_none(), "unarmed");
        assert!(route(true, Some(&a), &hit(34)).is_none(), "unmapped");
        let p = route(true, Some(&a), &hit(33)).expect("mapped");
        let s = p.sound().expect("a sounding part");
        assert_eq!((s.gain, s.pan), (0.7, -0.4));
        assert!(Arc::ptr_eq(&s.buf, p.buf.as_ref().unwrap()));
    }

    /// A source that yields `hits` on every read, then fails if `fails`.
    struct Fake {
        file: std::fs::File,
        hits: Vec<Hit>,
        fails: bool,
    }

    impl Source for Fake {
        fn name(&self) -> &str {
            "fake"
        }
        fn fd(&self) -> BorrowedFd<'_> {
            use std::os::fd::AsFd;
            self.file.as_fd()
        }
        fn read(&mut self, into: &mut Vec<Hit>) -> io::Result<()> {
            into.extend(&self.hits);
            if self.fails {
                Err(io::Error::from_raw_os_error(libc::ENODEV))
            } else {
                Ok(())
            }
        }
    }

    fn fake_with(hits: Vec<Hit>, fails: bool) -> Box<dyn Source> {
        Box::new(Fake {
            file: std::fs::File::open("/dev/null").unwrap(),
            hits,
            fails,
        })
    }

    fn fake() -> Box<dyn Source> {
        fake_with(Vec::new(), false)
    }

    fn nodes(names: &[&str]) -> Vec<Node> {
        names
            .iter()
            .map(|n| Node {
                path: Path::new("/dev/input").join(n),
                ino: 1,
            })
            .collect()
    }

    fn paths(names: &[&str]) -> Vec<PathBuf> {
        nodes(names).into_iter().map(|n| n.path).collect()
    }

    #[test]
    fn walk_status_follows_what_opened() {
        let mut sources = Vec::new();
        let mut rejected = HashSet::new();
        let status = walk(
            nodes(&["event0", "event1"]),
            &mut sources,
            &mut rejected,
            |p| {
                if p.ends_with("event1") {
                    Ok(Some(fake()))
                } else {
                    Ok(None)
                }
            },
        );
        assert_eq!(status, InputStatus::Devices(1));

        let mut sources = Vec::new();
        let mut rejected = HashSet::new();
        let status = walk(
            nodes(&["event0", "event1"]),
            &mut sources,
            &mut rejected,
            |p| {
                if p.ends_with("event0") {
                    Err(io::ErrorKind::NotFound.into())
                } else {
                    Err(io::ErrorKind::PermissionDenied.into())
                }
            },
        );
        assert_eq!(status, InputStatus::NoAccess("/dev/input/event1".into()));

        let mut sources = Vec::new();
        let mut rejected = HashSet::new();
        let status = walk(nodes(&["event0"]), &mut sources, &mut rejected, |_| {
            Ok(None)
        });
        assert_eq!(status, InputStatus::None);
        assert_eq!(
            walk(Vec::new(), &mut sources, &mut rejected, |_| Ok(None)),
            InputStatus::None
        );
    }

    #[test]
    fn walk_never_reopens_an_open_path_and_a_keyboard_outranks_a_refusal() {
        let mut sources = Vec::new();
        let mut rejected = HashSet::new();
        let mut opened = Vec::new();
        let mut open = |p: &Path| {
            opened.push(p.to_path_buf());
            if p.ends_with("event2") {
                Err(io::ErrorKind::PermissionDenied.into())
            } else {
                Ok(Some(fake()))
            }
        };
        assert_eq!(
            walk(nodes(&["event0"]), &mut sources, &mut rejected, &mut open),
            InputStatus::Devices(1)
        );
        assert_eq!(
            walk(
                nodes(&["event0", "event1", "event2"]),
                &mut sources,
                &mut rejected,
                &mut open
            ),
            InputStatus::Devices(2)
        );
        assert_eq!(opened, paths(&["event0", "event1", "event2"]));
    }

    #[test]
    fn walk_skips_a_rejected_node_until_it_is_replaced_and_retries_errors() {
        let mut sources = Vec::new();
        let mut rejected = HashSet::new();
        let mut opened = Vec::new();
        let mut open = |p: &Path| {
            opened.push(p.to_path_buf());
            if p.ends_with("event1") {
                Err(io::ErrorKind::PermissionDenied.into())
            } else {
                Ok(None)
            }
        };
        let both = nodes(&["event0", "event1"]);
        walk(both.clone(), &mut sources, &mut rejected, &mut open);
        walk(both.clone(), &mut sources, &mut rejected, &mut open);
        let mut replaced = both.clone();
        replaced[0].ino = 2;
        walk(replaced, &mut sources, &mut rejected, &mut open);
        walk(nodes(&["event1"]), &mut sources, &mut rejected, &mut open);
        walk(both, &mut sources, &mut rejected, &mut open);
        assert_eq!(
            opened,
            paths(&[
                "event0", "event1", // first walk
                "event1", // event0 rejected; the refusal is retried
                "event0", "event1", // event0 is a new node
                "event1", // event0 gone, pruned
                "event0", "event1", // event0 back
            ])
        );
    }

    #[test]
    fn a_failed_read_drops_only_that_source_and_the_next_walk_reopens_it() {
        let mut sources = Vec::new();
        let mut rejected = HashSet::new();
        let all = nodes(&["event0", "event1", "event2", "event3"]);
        let names = paths(&["event0", "event1", "event2", "event3"]);
        let mut opened: Vec<PathBuf> = Vec::new();
        // Device N strikes key 30 + N on every read; event1 and event3 fail
        // on their first opening's read.
        let mut open = |p: &Path| {
            let n = names.iter().position(|q| q == p).unwrap() as u16;
            let fails = n % 2 == 1 && !opened.iter().any(|q| q == p);
            opened.push(p.to_path_buf());
            Ok(Some(fake_with(vec![hit(30 + n)], fails)))
        };
        walk(all.clone(), &mut sources, &mut rejected, &mut open);
        let mut hits = Vec::new();
        read_ready(&mut sources, &[true, true, false, true], &mut hits);
        let left: Vec<&Path> = sources.iter().map(|s| s.path.as_path()).collect();
        assert_eq!(
            left,
            [
                Path::new("/dev/input/event0"),
                Path::new("/dev/input/event2")
            ]
        );
        assert_eq!(
            hits,
            vec![hit(30), hit(31), hit(33)],
            "strokes read before a failure are kept"
        );

        assert_eq!(
            walk(all, &mut sources, &mut rejected, &mut open),
            InputStatus::Devices(4)
        );
        assert_eq!(
            opened,
            paths(&["event0", "event1", "event2", "event3", "event1", "event3"])
        );
    }

    #[test]
    fn focus_counts_until_its_deadline_or_always_without_one() {
        let shared = Shared::default();
        assert!(
            !shared.focused_at(Mono(0)),
            "unfocused until the window says so"
        );
        shared.set_focus(true, None);
        assert!(shared.focused_at(Mono(u64::MAX)));
        shared.set_focus(true, Some(Mono(1_000)));
        assert!(shared.focused_at(Mono(1_000)));
        assert!(!shared.focused_at(Mono(1_001)), "no pass since: hidden");
        shared.set_focus(false, None);
        assert!(!shared.focused_at(Mono(0)));
    }

    #[test]
    fn event_nodes_lists_only_event_nodes_sorted() {
        let dir = std::env::temp_dir().join(format!("djembot-input-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for n in ["event3", "mice", "event10", "by-id"] {
            std::fs::write(dir.join(n), b"").unwrap();
        }
        let got: Vec<PathBuf> = event_nodes(&dir).into_iter().map(|n| n.path).collect();
        assert_eq!(got, vec![dir.join("event10"), dir.join("event3")]);
        assert!(event_nodes(&dir.join("missing")).is_empty());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn status_texts() {
        assert_eq!(InputStatus::Devices(1).status_text(), "keys: 1 device");
        assert_eq!(InputStatus::Devices(2).status_text(), "keys: 2 devices");
        assert_eq!(
            InputStatus::NoAccess("/dev/input/event0".into()).status_text(),
            "keys: no access to /dev/input/event0 — add yourself to the input group and log in again"
        );
        assert_eq!(InputStatus::None.status_text(), "keys: no keyboard found");
    }

    #[test]
    fn dispatch_sounds_and_forwards_mapped_strokes_only() {
        let shared = Shared::default();
        shared.set_arming(Some(armed()));
        let (mut prod, mut cons) = rtrb::RingBuffer::new(1);
        let (tx, rx) = mpsc::channel();
        let now = Mono(5_000);
        let mut hits = vec![hit(33)];
        assert!(!dispatch(&mut hits, &mut prod, &tx, &shared, now));
        assert!(rx.try_recv().is_err(), "unfocused");
        assert!(cons.pop().is_err());

        shared.set_focus(true, Some(Mono(4_999)));
        let mut hits = vec![hit(33)];
        assert!(!dispatch(&mut hits, &mut prod, &tx, &shared, now));
        assert!(rx.try_recv().is_err(), "focus from a pass too long ago");
        assert!(cons.pop().is_err());

        shared.set_focus(true, Some(now));
        let mut hits = vec![hit(34), hit(33), hit(33)];
        assert!(!dispatch(&mut hits, &mut prod, &tx, &shared, now));
        assert!(hits.is_empty());
        let got: Vec<_> = rx.try_iter().collect();
        assert_eq!(got.len(), 2, "both mapped strokes reach the window");
        assert_eq!(got[0].0, hit(33));
        assert!(cons.pop().is_ok());
        assert!(
            cons.pop().is_err(),
            "the full ring dropped the second sound"
        );

        let mut silent = part();
        silent.buf = None;
        assert!(silent.sound().is_none());
        shared.set_arming(Some(Arming {
            parts: HashMap::from([(Trigger::Key(35), silent)]),
        }));
        let mut hits = vec![hit(35)];
        assert!(!dispatch(&mut hits, &mut prod, &tx, &shared, now));
        let got: Vec<_> = rx.try_iter().collect();
        assert_eq!(
            got.len(),
            1,
            "a silent part's stroke still reaches the window"
        );
        assert_eq!(got[0].0, hit(35));
        assert!(got[0].1.buf.is_none());
        assert!(cons.pop().is_err(), "and sounds nothing");

        drop(rx);
        let mut hits = vec![hit(35)];
        assert!(dispatch(&mut hits, &mut prod, &tx, &shared, now));
    }
}
