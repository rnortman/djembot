//! The socket between the CLI and the window: one JSON request line, one
//! JSON response line per connection; a `wait` connection then carries one
//! JSON event line per transport event until a terminal one.

use std::ffi::OsString;
use std::fs::DirBuilder;
use std::io::{BufRead, BufReader, ErrorKind, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::lesson::Lesson;
use crate::perf::{EndedBy, Run};

/// A request's deadline after it arrives. The UI drops a request it reaches
/// later than this; the listener waits a fifth longer for the reply.
pub const REPLY_TIMEOUT: Duration = Duration::from_secs(5);
/// How long either side waits on a silent peer.
const IO_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "lowercase")]
pub enum Request {
    /// Replace the document with `lesson` (already validated, `file`
    /// references expanded), optionally starting block `play`.
    Show {
        /// The lesson's display name: the absolute path, or `<stdin>`.
        path: String,
        lesson: Lesson,
        play: Option<String>,
    },
    /// Play block `label`, or the first block when `None`.
    Play {
        label: Option<String>,
    },
    Stop,
    /// The run of the open performance, else of the last one closed.
    Report,
    /// Show the built-in calibration lesson and play its block.
    Calibrate,
    /// Stream transport events: the state now, then every event, until a
    /// terminal one.
    Wait,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Response {
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub labels: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// The run a `report` asked for.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run: Option<Run>,
    /// Where that run's file is; absent while it is open or when it was not
    /// written.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_path: Option<String>,
    /// Problems that did not stop the request.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
    /// The block a `show --play` or `play` armed instead of playing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub armed: Option<String>,
}

impl Response {
    pub fn ok(labels: Vec<String>) -> Response {
        Response {
            ok: true,
            labels: Some(labels),
            ..Response::default()
        }
    }

    pub fn err(error: impl Into<String>) -> Response {
        Response {
            ok: false,
            error: Some(error.into()),
            ..Response::default()
        }
    }

    pub fn run(run: Run, path: Option<String>) -> Response {
        Response {
            ok: true,
            run: Some(run),
            run_path: path,
            ..Response::default()
        }
    }
}

/// What a transport event reports: `shown`, `armed`, `started`, `ended`;
/// as the state a `wait` begins with, also `playing`, `count-in`, `paused`
/// or `idle`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum EventKind {
    Shown,
    Armed,
    Started,
    Ended,
    Playing,
    CountIn,
    Paused,
    #[default]
    Idle,
}

impl EventKind {
    /// The name `wait` prints it with.
    pub fn as_str(self) -> &'static str {
        match self {
            EventKind::Shown => "shown",
            EventKind::Armed => "armed",
            EventKind::Started => "started",
            EventKind::Ended => "ended",
            EventKind::Playing => "playing",
            EventKind::CountIn => "count-in",
            EventKind::Paused => "paused",
            EventKind::Idle => "idle",
        }
    }
}

/// A transport event of the window, as `wait` streams it.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Event {
    pub event: EventKind,
    /// The block.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// `shown`: the lesson's path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lesson: Option<String>,
    /// `ended`: what ended the block.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub by: Option<EndedBy>,
    /// `ended`: the block the score went on into.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next: Option<String>,
    /// `playing`: the 1-based cycle being heard.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cycle: Option<u32>,
    /// `ended`: the cycles played.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cycles_played: Option<u32>,
    /// The block's `cycles`; absent for a looping block.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cycles: Option<u32>,
    /// `ended` of a block with a part: its tally.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hit: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub miss: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extra: Option<usize>,
    /// `ended`: the run file, if one was written.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run: Option<String>,
    /// The performance is over and nothing follows it: a `wait` ends here.
    #[serde(default)]
    pub terminal: bool,
}

impl Event {
    pub fn new(event: EventKind, label: Option<String>) -> Event {
        Event {
            event,
            label,
            ..Event::default()
        }
    }

    /// The event as `wait` prints it: the event, then `key=value` in a fixed
    /// order with `run=` last. A value that is empty or holds a space, a
    /// quote or `=` is printed as a JSON string.
    pub fn line(&self) -> String {
        let mut out = self.event.as_str().to_string();
        let mut put = |key: &str, value: Option<String>| {
            if let Some(v) = value {
                out.push_str(&format!(" {key}={}", quoted(&v)));
            }
        };
        let of = |n: u32| match self.cycles {
            Some(m) => format!("{n}/{m}"),
            None => n.to_string(),
        };
        put("label", self.label.clone());
        put("lesson", self.lesson.clone());
        put("by", self.by.map(|b| b.as_str().to_string()));
        put("next", self.next.clone());
        put("cycle", self.cycle.map(of));
        put("cycles", self.cycles_played.map(of));
        put("hit", self.hit.map(|n| n.to_string()));
        put("miss", self.miss.map(|n| n.to_string()));
        put("extra", self.extra.map(|n| n.to_string()));
        put("run", self.run.clone());
        out
    }
}

fn quoted(v: &str) -> String {
    if v.is_empty() || v.contains(|c: char| c.is_whitespace() || c == '"' || c == '=') {
        serde_json::to_string(v).expect("strings serialize")
    } else {
        v.to_string()
    }
}

/// A request waiting for the UI thread, with the channel its reply goes to.
/// Once `deadline` has passed the client has been told the gui did not
/// respond, so the UI drops an expired request without acting on it.
pub struct Pending {
    pub request: Request,
    pub reply: mpsc::SyncSender<Response>,
    pub deadline: Instant,
    /// A `wait`'s event stream: the UI sends it every event until a
    /// terminal one; the connection writes each to the client.
    pub events: Option<mpsc::Sender<Event>>,
}

impl Pending {
    pub fn expired(&self) -> bool {
        Instant::now() >= self.deadline
    }
}

/// `$XDG_RUNTIME_DIR/djembot.sock`, else `/tmp/djembot-<uid>/djembot.sock` in
/// a directory that must be private to this user.
pub fn socket_path() -> Result<PathBuf> {
    let uid = real_uid();
    let path = socket_path_from(std::env::var_os("XDG_RUNTIME_DIR"), uid);
    if let Some(dir) = path.parent()
        && dir == fallback_dir(uid)
    {
        private_dir(dir, uid)?;
    }
    Ok(path)
}

/// The real uid of this process; `getuid` cannot fail.
fn real_uid() -> u32 {
    unsafe extern "C" {
        safe fn getuid() -> u32;
    }
    getuid()
}

fn fallback_dir(uid: u32) -> PathBuf {
    PathBuf::from(format!("/tmp/djembot-{uid}"))
}

fn socket_path_from(runtime_dir: Option<OsString>, uid: u32) -> PathBuf {
    match runtime_dir.filter(|d| !d.is_empty()) {
        Some(d) => PathBuf::from(d),
        None => fallback_dir(uid),
    }
    .join("djembot.sock")
}

/// Creates `dir` with mode 0700 if it is missing, then requires it to be a
/// real directory (not a symlink) owned by `uid` with mode 0700, so no other
/// user can plant or reach a socket in it.
fn private_dir(dir: &Path, uid: u32) -> Result<()> {
    match DirBuilder::new().mode(0o700).create(dir) {
        Err(e) if e.kind() != ErrorKind::AlreadyExists => {
            return Err(e).with_context(|| format!("creating {}", dir.display()));
        }
        _ => {}
    }
    let meta =
        std::fs::symlink_metadata(dir).with_context(|| format!("inspecting {}", dir.display()))?;
    if !meta.is_dir() || meta.uid() != uid || meta.mode() & 0o777 != 0o700 {
        bail!(
            "{} is not a directory owned by uid {uid} with mode 0700; remove it, or set \
             XDG_RUNTIME_DIR",
            dir.display()
        );
    }
    Ok(())
}

/// Sends `req` to the window listening at `path` and returns its response.
pub fn send(path: &Path, req: &Request) -> Result<Response> {
    exchange(path, req).map(|(response, _)| response)
}

/// Sends `req` (a `wait`) and returns the response and the stream the
/// events follow on, with no read timeout: the next event may be minutes
/// away.
pub fn send_streaming(path: &Path, req: &Request) -> Result<(Response, BufReader<UnixStream>)> {
    let (response, reader) = exchange(path, req)?;
    reader.get_ref().set_read_timeout(None)?;
    Ok((response, reader))
}

/// Connects, writes `req` and reads the response line.
fn exchange(path: &Path, req: &Request) -> Result<(Response, BufReader<UnixStream>)> {
    let mut stream = match UnixStream::connect(path) {
        Ok(s) => s,
        Err(e) if matches!(e.kind(), ErrorKind::NotFound | ErrorKind::ConnectionRefused) => {
            bail!(
                "no `djembot gui` is listening at {} — start it in a terminal inside your graphical session",
                path.display()
            )
        }
        Err(e) => return Err(e).with_context(|| format!("connecting to {}", path.display())),
    };
    stream.set_read_timeout(Some(IO_TIMEOUT))?;
    stream.set_write_timeout(Some(IO_TIMEOUT))?;
    let mut line = serde_json::to_string(req)?;
    line.push('\n');
    stream
        .write_all(line.as_bytes())
        .with_context(|| format!("writing to {}", path.display()))?;
    let mut reply = String::new();
    let mut reader = BufReader::new(stream);
    reader
        .read_line(&mut reply)
        .with_context(|| format!("reading the reply from {}", path.display()))?;
    if reply.is_empty() {
        bail!(
            "`djembot gui` at {} closed the connection without replying",
            path.display()
        );
    }
    let response = serde_json::from_str(&reply).context("the window sent an unreadable reply")?;
    Ok((response, reader))
}

/// Removes the socket file when dropped.
#[derive(Debug)]
pub struct SocketFile(PathBuf);

impl Drop for SocketFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Binds the socket at `path`. A leftover file nobody listens on is
/// removed first; a live listener is an error.
pub fn bind(path: &Path) -> Result<(UnixListener, SocketFile)> {
    let listener = match UnixListener::bind(path) {
        Err(e) if e.kind() == ErrorKind::AddrInUse => {
            if UnixStream::connect(path).is_ok() {
                bail!(
                    "another djembot gui is running (listening at {})",
                    path.display()
                );
            }
            std::fs::remove_file(path)
                .with_context(|| format!("removing the stale socket {}", path.display()))?;
            UnixListener::bind(path)
        }
        r => r,
    }
    .with_context(|| format!("binding {}", path.display()))?;
    Ok((listener, SocketFile(path.to_path_buf())))
}

/// Spawns the listener thread. Each connection is answered on a thread of
/// its own, so its request is read, and given its deadline, as soon as it
/// arrives: it goes to `requests` with a deadline `timeout` away, `wake` is
/// called with that deadline so the UI notices, and the UI's reply (or a
/// timeout error `timeout` plus a fifth after arrival) is written back.
/// `wake` should return by the deadline: the reply wait counts from arrival,
/// so time spent in `wake` comes out of it.
pub fn serve(
    listener: UnixListener,
    requests: mpsc::Sender<Pending>,
    wake: impl Fn(Instant) + Send + Sync + 'static,
    timeout: Duration,
) -> std::thread::JoinHandle<()> {
    let wake = Arc::new(wake);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            match stream {
                Ok(s) => {
                    let requests = requests.clone();
                    let wake = wake.clone();
                    std::thread::spawn(move || handle(s, &requests, &*wake, timeout));
                }
                Err(e) => eprintln!("djembot: socket accept failed: {e}"),
            }
        }
    })
}

/// Answers one connection. An accepted `wait` then writes each event the
/// UI sends, one JSON line each, until a terminal one, a failed write (the
/// client went away) or the UI dropping the sender (the window is closing).
fn handle(
    stream: UnixStream,
    requests: &mpsc::Sender<Pending>,
    wake: &impl Fn(Instant),
    timeout: Duration,
) {
    let _ = stream.set_read_timeout(Some(IO_TIMEOUT));
    let _ = stream.set_write_timeout(Some(IO_TIMEOUT));
    let mut line = String::new();
    let mut reader = BufReader::new(&stream);
    let mut events = None;
    let response = match reader.read_line(&mut line) {
        // A probe (`bind` checking for a live window) sends nothing.
        Ok(0) => return,
        Err(e) => Response::err(format!("reading the request: {e}")),
        Ok(_) => match serde_json::from_str::<Request>(&line) {
            Err(e) => Response::err(format!("unreadable request: {e}")),
            Ok(request) => {
                let (reply, answer) = mpsc::sync_channel(1);
                let deadline = Instant::now() + timeout;
                let sender = (request == Request::Wait).then(|| {
                    let (tx, rx) = mpsc::channel();
                    events = Some(rx);
                    tx
                });
                let pending = Pending {
                    request,
                    reply,
                    deadline,
                    events: sender,
                };
                if requests.send(pending).is_err() {
                    Response::err("gui is shutting down")
                } else {
                    // The extra fifth lets a request the UI took up just
                    // before `deadline` still report its outcome.
                    let reply_by = deadline + timeout / 5;
                    wake(deadline);
                    answer
                        .recv_timeout(reply_by.saturating_duration_since(Instant::now()))
                        .unwrap_or_else(|_| Response::err("gui did not respond"))
                }
            }
        },
    };
    if write_line(&stream, &response).is_err() || !response.ok {
        return;
    }
    for event in events.iter().flatten() {
        if write_line(&stream, &event).is_err() || event.terminal {
            return;
        }
    }
}

fn write_line(mut stream: &UnixStream, value: &impl Serialize) -> std::io::Result<()> {
    let mut out = serde_json::to_string(value).expect("replies serialize");
    out.push('\n');
    stream.write_all(out.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lesson::Segment;
    use crate::timeline::tests::exercise;
    use serde_json::json;

    fn tmpdir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("djembot-ipc-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn lesson() -> Lesson {
        Lesson {
            segments: vec![
                Segment::Text("# Clave\n".into()),
                Segment::Rhythm(exercise(json!({
                    "label": "A", "tempo": 90, "beats": 4, "cycles": 2,
                    "voices": [{ "name": "clave", "instrument": "clave", "pulses": 16,
                                 "onsets": [0, 3, 6, {"at": 10, "accent": true}, 12] }]
                }))),
                Segment::Rhythm(exercise(json!({
                    "label": "B", "tempo": 60, "beats": 2,
                    "voices": [{ "name": "bell", "instrument": "bell", "pulses": 3,
                                 "onsets": [{"at": 0, "tone": "l"}, 1, 2], "pan": -0.5 },
                               { "name": "x", "instrument": "gm:42", "pulses": 2,
                                 "onsets": [0], "mute": true }]
                }))),
            ],
        }
    }

    /// A stub UI thread: records each request and answers with the labels
    /// of the lesson it was shown, or an error for `stop`.
    fn stub(dir: &Path) -> (PathBuf, SocketFile, mpsc::Receiver<Request>) {
        let path = dir.join("djembot.sock");
        let (listener, file) = bind(&path).unwrap();
        let (tx, rx) = mpsc::channel::<Pending>();
        let (seen_tx, seen) = mpsc::channel();
        serve(listener, tx, |_| {}, REPLY_TIMEOUT);
        std::thread::spawn(move || {
            for p in rx {
                assert!(!p.expired());
                let reply = match &p.request {
                    Request::Show { lesson, .. } => Response::ok(lesson.labels()),
                    Request::Play { .. } => Response::ok(vec![]),
                    Request::Stop => Response::err("nothing is playing"),
                    Request::Report | Request::Calibrate | Request::Wait => {
                        Response::err("no performance yet")
                    }
                };
                seen_tx.send(p.request).unwrap();
                p.reply.send(reply).unwrap();
            }
        });
        (path, file, seen)
    }

    #[test]
    fn socket_path_prefers_the_runtime_dir() {
        assert_eq!(
            socket_path_from(Some("/run/user/1000".into()), 1000),
            PathBuf::from("/run/user/1000/djembot.sock")
        );
        assert_eq!(
            socket_path_from(None, 1000),
            PathBuf::from("/tmp/djembot-1000/djembot.sock")
        );
        assert_eq!(
            socket_path_from(Some("".into()), 7),
            PathBuf::from("/tmp/djembot-7/djembot.sock")
        );
    }

    #[test]
    fn fallback_dir_must_be_private() {
        use std::os::unix::fs::PermissionsExt;
        let uid = real_uid();
        let base = tmpdir("private");
        let dir = base.join("d");
        private_dir(&dir, uid).unwrap();
        let meta = std::fs::symlink_metadata(&dir).unwrap();
        assert_eq!(meta.mode() & 0o777, 0o700);
        // Existing and private: accepted again.
        private_dir(&dir, uid).unwrap();
        // Someone else's uid.
        let err = private_dir(&dir, uid + 1).unwrap_err().to_string();
        assert!(err.contains("not a directory owned by uid"), "{err}");
        // Readable by others.
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(private_dir(&dir, uid).is_err());
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        // A symlink to a private directory.
        let link = base.join("link");
        std::os::unix::fs::symlink(&dir, &link).unwrap();
        assert!(private_dir(&link, uid).is_err());
    }

    #[test]
    fn wire_format() {
        let req = Request::Play {
            label: Some("A".into()),
        };
        assert_eq!(
            serde_json::to_value(&req).unwrap(),
            json!({"cmd": "play", "label": "A"})
        );
        assert_eq!(
            serde_json::from_value::<Request>(json!({"cmd": "play", "label": null})).unwrap(),
            Request::Play { label: None }
        );
        assert_eq!(
            serde_json::from_value::<Request>(json!({"cmd": "stop"})).unwrap(),
            Request::Stop
        );
        assert_eq!(
            serde_json::to_value(Response::ok(vec!["A".into(), "B".into()])).unwrap(),
            json!({"ok": true, "labels": ["A", "B"]})
        );
        assert_eq!(
            serde_json::to_value(Response::err("unknown label")).unwrap(),
            json!({"ok": false, "error": "unknown label"})
        );
    }

    #[test]
    fn report_and_calibrate_wire_format() {
        assert_eq!(
            serde_json::to_value(Request::Report).unwrap(),
            json!({"cmd": "report"})
        );
        assert_eq!(
            serde_json::from_value::<Request>(json!({"cmd": "calibrate"})).unwrap(),
            Request::Calibrate
        );
        let warned = Response {
            warnings: vec!["no keyboard".into()],
            ..Response::ok(vec!["A".into()])
        };
        let value = serde_json::to_value(&warned).unwrap();
        assert_eq!(
            value,
            json!({"ok": true, "labels": ["A"], "warnings": ["no keyboard"]})
        );
        assert_eq!(serde_json::from_value::<Response>(value).unwrap(), warned);

        let run: Run = serde_json::from_value(json!({
            "djembot": 1, "lesson": "<stdin>", "label": "A", "title": null,
            "tempo": 90.0, "beats": 4, "cycles": null, "count_in": 0,
            "window_ms": 100.0, "offset_ms": 0.0, "input": "1 device",
            "started": "2026-10-11T10:12:03Z", "ended": "2026-10-11T10:12:04Z",
            "ended_by": "open", "cycles_played": 1, "parts": [],
            "hits": [{"t_ms": 10.0, "voice": 1, "tone": "x", "cycle": 0, "pulse": 0,
                      "target_tone": "x", "error_ms": 10.0, "judgment": "hit"}],
            "misses": [{"cycle": 0, "voice": 1, "pulse": 4}]
        }))
        .unwrap();
        let reply = Response::run(run.clone(), Some("/runs/x.json".into()));
        let value = serde_json::to_value(&reply).unwrap();
        assert_eq!(value["run"]["ended_by"], "open");
        assert_eq!(value["run_path"], "/runs/x.json");
        assert!(value.get("labels").is_none() && value.get("warnings").is_none());
        assert_eq!(serde_json::from_value::<Response>(value).unwrap(), reply);
        // A reply from an older window, without the new fields, still reads.
        let old: Response = serde_json::from_value(json!({"ok": true, "labels": []})).unwrap();
        assert_eq!(old, Response::ok(vec![]));
    }

    #[test]
    fn round_trip() {
        let dir = tmpdir("round-trip");
        let (path, _file, seen) = stub(&dir);
        let show = Request::Show {
            path: "/abs/lessons/003-son.md".into(),
            lesson: lesson(),
            play: Some("B".into()),
        };
        let r = send(&path, &show).unwrap();
        assert_eq!(r, Response::ok(vec!["A".into(), "B".into()]));
        assert_eq!(seen.recv().unwrap(), show);

        let r = send(&path, &Request::Play { label: None }).unwrap();
        assert!(r.ok);
        assert_eq!(seen.recv().unwrap(), Request::Play { label: None });

        let r = send(&path, &Request::Stop).unwrap();
        assert_eq!(r, Response::err("nothing is playing"));
    }

    #[test]
    fn unreadable_request_gets_an_error_reply() {
        let dir = tmpdir("garbage");
        let (path, _file, _seen) = stub(&dir);
        let mut s = UnixStream::connect(&path).unwrap();
        s.write_all(b"{\"cmd\": \"dance\"}\n").unwrap();
        let mut line = String::new();
        BufReader::new(s).read_line(&mut line).unwrap();
        let r: Response = serde_json::from_str(&line).unwrap();
        assert!(!r.ok);
        assert!(r.error.unwrap().contains("unreadable request"));
    }

    #[test]
    fn silent_ui_times_out() {
        let dir = tmpdir("timeout");
        let path = dir.join("djembot.sock");
        let (listener, _file) = bind(&path).unwrap();
        let (tx, rx) = mpsc::channel::<Pending>();
        serve(listener, tx, |_| {}, Duration::from_millis(50));
        let r = send(&path, &Request::Stop).unwrap();
        assert_eq!(r, Response::err("gui did not respond"));
        // The abandoned request is still queued, marked expired.
        let p = rx.try_recv().unwrap();
        assert_eq!(p.request, Request::Stop);
        assert!(p.expired());
    }

    /// Time spent in `wake` comes out of the reply wait: a wake that returns
    /// at the deadline still gets `gui did not respond` a fifth later.
    #[test]
    fn slow_wake_does_not_extend_the_reply_wait() {
        let dir = tmpdir("slowwake");
        let path = dir.join("djembot.sock");
        let (listener, _file) = bind(&path).unwrap();
        let (tx, _rx) = mpsc::channel::<Pending>();
        let timeout = Duration::from_millis(500);
        serve(
            listener,
            tx,
            |deadline| std::thread::sleep(deadline.saturating_duration_since(Instant::now())),
            timeout,
        );
        let sent = Instant::now();
        let r = send(&path, &Request::Stop).unwrap();
        assert_eq!(r, Response::err("gui did not respond"));
        let took = sent.elapsed();
        assert!(took >= timeout, "{took:?}");
        assert!(took < timeout + timeout / 2, "{took:?}");
    }

    /// Concurrent requests each get their deadline on arrival, not after the
    /// one ahead of them has timed out.
    #[test]
    fn concurrent_requests_are_queued_on_arrival() {
        let dir = tmpdir("concurrent");
        let path = dir.join("djembot.sock");
        let (listener, _file) = bind(&path).unwrap();
        let (tx, rx) = mpsc::channel::<Pending>();
        let timeout = Duration::from_millis(500);
        serve(listener, tx, |_| {}, timeout);
        let sent = Instant::now();
        let clients: Vec<_> = (0..3)
            .map(|_| {
                let path = path.clone();
                std::thread::spawn(move || send(&path, &Request::Stop).unwrap())
            })
            .collect();
        // The UI answers nothing; every request reaches it before the first
        // has timed out, with a deadline `timeout` after it was sent.
        for _ in 0..3 {
            let p = rx.recv_timeout(timeout / 2).unwrap();
            assert!(p.deadline < sent + timeout + timeout / 2);
        }
        for c in clients {
            assert_eq!(c.join().unwrap(), Response::err("gui did not respond"));
        }
    }

    #[test]
    fn wake_is_called_per_request() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};
        let dir = tmpdir("wake");
        let path = dir.join("djembot.sock");
        let (listener, _file) = bind(&path).unwrap();
        let (tx, rx) = mpsc::channel::<Pending>();
        let woken = Arc::new(AtomicUsize::new(0));
        let w = woken.clone();
        serve(
            listener,
            tx,
            move |_| {
                w.fetch_add(1, Ordering::SeqCst);
            },
            REPLY_TIMEOUT,
        );
        std::thread::spawn(move || {
            for p in rx {
                p.reply.send(Response::ok(vec![])).unwrap();
            }
        });
        send(&path, &Request::Stop).unwrap();
        send(&path, &Request::Stop).unwrap();
        assert_eq!(woken.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn client_error_when_nothing_listens() {
        let dir = tmpdir("nobody");
        let path = dir.join("djembot.sock");
        let msg = send(&path, &Request::Stop).unwrap_err().to_string();
        assert_eq!(
            msg,
            format!(
                "no `djembot gui` is listening at {} — start it in a terminal inside your graphical session",
                path.display()
            )
        );
        // A stale socket file (connect refused) reads the same.
        drop(UnixListener::bind(&path).unwrap());
        assert!(path.exists());
        let stale = send(&path, &Request::Stop).unwrap_err().to_string();
        assert_eq!(stale, msg);
    }

    /// A UI thread that answers one `wait` with `ok`, then hands its event
    /// sender to the test.
    fn waiting(dir: &Path) -> (PathBuf, SocketFile, mpsc::Receiver<mpsc::Sender<Event>>) {
        let path = dir.join("djembot.sock");
        let (listener, file) = bind(&path).unwrap();
        let (tx, rx) = mpsc::channel::<Pending>();
        let (senders_tx, senders) = mpsc::channel();
        serve(listener, tx, |_| {}, REPLY_TIMEOUT);
        std::thread::spawn(move || {
            for p in rx {
                assert_eq!(p.request, Request::Wait);
                p.reply.send(Response::ok(vec![])).unwrap();
                senders_tx.send(p.events.unwrap()).unwrap();
            }
        });
        (path, file, senders)
    }

    fn ended(terminal: bool) -> Event {
        Event {
            by: Some(EndedBy::End),
            terminal,
            ..Event::new(EventKind::Ended, Some("A".into()))
        }
    }

    #[test]
    fn wait_streams_events_until_a_terminal_one() {
        let dir = tmpdir("wait");
        let (path, _file, senders) = waiting(&dir);
        let (response, mut reader) = send_streaming(&path, &Request::Wait).unwrap();
        assert!(response.ok);
        let events = senders.recv().unwrap();
        let started = Event::new(EventKind::Started, Some("A".into()));
        events.send(started.clone()).unwrap();
        events.send(ended(true)).unwrap();
        events.send(started.clone()).unwrap();
        let mut lines = Vec::new();
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line).unwrap() == 0 {
                break;
            }
            lines.push(serde_json::from_str::<Event>(&line).unwrap());
        }
        assert_eq!(
            lines,
            [started, ended(true)],
            "nothing after the terminal one"
        );
    }

    #[test]
    fn wait_ends_when_the_client_goes_away() {
        let dir = tmpdir("wait-gone");
        let (path, _file, senders) = waiting(&dir);
        let (response, reader) = send_streaming(&path, &Request::Wait).unwrap();
        assert!(response.ok);
        let events = senders.recv().unwrap();
        drop(reader);
        // The connection finds the client gone on its next write and drops
        // its receiver, so a later send fails.
        let gave_up = (0..500).any(|_| {
            std::thread::sleep(Duration::from_millis(10));
            events.send(ended(false)).is_err()
        });
        assert!(gave_up);
    }

    #[test]
    fn a_wait_ends_with_the_window() {
        let dir = tmpdir("wait-eof");
        let (path, _file, senders) = waiting(&dir);
        let (_, mut reader) = send_streaming(&path, &Request::Wait).unwrap();
        drop(senders.recv().unwrap());
        let mut line = String::new();
        assert_eq!(reader.read_line(&mut line).unwrap(), 0, "EOF");
    }

    #[test]
    fn event_lines_are_json_objects() {
        let e = Event {
            by: Some(EndedBy::End),
            next: Some("alone".into()),
            cycles_played: Some(8),
            cycles: Some(8),
            hit: Some(29),
            miss: Some(3),
            extra: Some(1),
            run: Some("/runs/x.json".into()),
            ..Event::new(EventKind::Ended, Some("guided".into()))
        };
        let value = serde_json::to_value(&e).unwrap();
        assert_eq!(
            value,
            json!({"event": "ended", "label": "guided", "by": "end", "next": "alone",
                   "cycles_played": 8, "cycles": 8, "hit": 29, "miss": 3, "extra": 1,
                   "run": "/runs/x.json", "terminal": false})
        );
        assert_eq!(serde_json::from_value::<Event>(value).unwrap(), e);
        assert_eq!(
            e.line(),
            "ended label=guided by=end next=alone cycles=8/8 hit=29 miss=3 extra=1 \
             run=/runs/x.json"
        );
        let playing = Event {
            cycle: Some(3),
            ..Event::new(EventKind::Playing, Some("block 2".into()))
        };
        assert_eq!(playing.line(), "playing label=\"block 2\" cycle=3");
        assert_eq!(Event::new(EventKind::Idle, None).line(), "idle");
        for kind in [
            EventKind::Shown,
            EventKind::Armed,
            EventKind::Started,
            EventKind::Ended,
            EventKind::Playing,
            EventKind::CountIn,
            EventKind::Paused,
            EventKind::Idle,
        ] {
            assert_eq!(serde_json::to_value(kind).unwrap(), json!(kind.as_str()));
        }
        assert_eq!(
            serde_json::to_value(Request::Wait).unwrap(),
            json!({"cmd": "wait"})
        );
    }

    #[test]
    fn stale_socket_is_replaced_and_live_one_refused() {
        let dir = tmpdir("stale");
        let path = dir.join("djembot.sock");
        drop(UnixListener::bind(&path).unwrap());
        assert!(path.exists());
        let (live, file) = bind(&path).unwrap();
        let err = bind(&path).unwrap_err().to_string();
        assert!(err.contains("another djembot gui is running"), "{err}");
        drop(live);
        drop(file);
        assert!(!path.exists(), "socket file removed on drop");
    }
}
