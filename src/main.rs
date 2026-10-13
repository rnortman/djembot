#[cfg(not(target_os = "linux"))]
compile_error!("djembot supports Linux only: it reads the keyboard through evdev");

mod engine;
mod fluid;
mod input;
mod ipc;
mod lesson;
mod mixer;
mod model;
mod perf;
mod report;
mod sounds;
mod starter;
mod timeline;
mod ui;

use std::io::{self, BufRead, Read, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::{Arc, Mutex, mpsc};

use anyhow::{Context, Result, anyhow};
use clap::{Parser, Subcommand};
use eframe::egui;

use engine::Mono;
use model::{Diagnostics, Exercise};

/// Djembot: an LLM instructor plays rhythms; a window shows them in sync.
#[derive(Parser)]
#[command(name = "djembot", version)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Open the window and listen for the other commands. Run it in a
    /// terminal inside your graphical session.
    Gui {
        /// GM soundfont to render the percussion from (default:
        /// $DJEMBOT_SOUNDFONT, else the fluid-soundfont-gm install).
        #[arg(long, value_name = "PATH")]
        soundfont: Option<PathBuf>,
    },
    /// Validate a lesson and show it in the window.
    Show {
        /// Lesson markdown file, or `-` to read it from stdin (`file`
        /// references then resolve against the current directory).
        lesson: PathBuf,
        /// Start this block (label or 1-based number) once shown; a score
        /// with a part in any of its blocks is armed instead, and the user
        /// starts it with space.
        #[arg(long, value_name = "LABEL")]
        play: Option<String>,
    },
    /// Play a block of the shown lesson (label or 1-based number; default
    /// the first block); a score with a part in any of its blocks is armed
    /// instead, and the user starts it with space.
    Play { label: Option<String> },
    /// Stop playback.
    Stop,
    /// Wait for the performance to end: print the window's state, then each
    /// transport event (`started`, `ended`, …) as it happens, and exit once
    /// one ends the performance (stopped, or played to the end of its score).
    Wait,
    /// Print the report of the current or last performance (or of
    /// RUN.json); `--raw` prints the run JSON instead.
    Report {
        /// A run file; no window is needed then.
        run: Option<PathBuf>,
        /// Print the run JSON instead of the report.
        #[arg(long)]
        raw: bool,
    },
    /// Measure your tap offset: play the calibration block and tap `f` on
    /// every click.
    Calibrate,
    /// Check a lesson (.md) or a single exercise (.json); print every error.
    Validate {
        /// Lesson markdown or exercise JSON file, or `-` to read stdin (a
        /// lesson unless `--exercise`).
        file: PathBuf,
        /// Treat the input as a single exercise JSON, whatever its name.
        #[arg(long)]
        exercise: bool,
    },
    /// Lay the instructor contract (as CLAUDE.md), a journal, the library and
    /// a first lesson into a learning directory; an existing file is kept.
    Init {
        /// The learning directory (default: the current one); created if
        /// absent.
        dir: Option<PathBuf>,
    },
}

fn main() -> ExitCode {
    match Cli::parse().command {
        Command::Gui { soundfont } => match gui(soundfont.as_deref()) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("djembot gui: {e:#}");
                ExitCode::FAILURE
            }
        },
        Command::Show { lesson, play } => show(&Input::from(&lesson), play),
        Command::Play { label } => request(&ipc::Request::Play { label }),
        Command::Stop => request(&ipc::Request::Stop),
        Command::Wait => wait(),
        Command::Report { run, raw } => report_run(run.as_deref(), raw),
        Command::Calibrate => request(&ipc::Request::Calibrate),
        Command::Validate { file, exercise } => validate(&Input::from(&file), exercise),
        Command::Init { dir } => init(dir.as_deref().unwrap_or(Path::new("."))),
    }
}

/// Runs `starter::init` into `dir`, printing its lines as it goes.
fn init(dir: &Path) -> ExitCode {
    match starter::init(dir, &mut io::stdout().lock()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            println!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Opens the audio stream, renders the sound bank, binds the socket and runs
/// the window until it is closed. The socket file is removed on return.
fn gui(soundfont: Option<&Path>) -> Result<()> {
    let soundfont = sounds::find_soundfont(soundfont)?;
    let socket = ipc::socket_path()?;
    let (listener, _socket_file) = ipc::bind(&socket)?;
    let mut engine = engine::Engine::new()?;
    let engine_hits = engine
        .take_hit_producer()
        .expect("a fresh engine still has its hit producer");
    let synth = fluid::Synth::new(engine.sample_rate(), &soundfont)?;
    let bank = sounds::Bank::new(synth)?;
    let (requests, rx) = mpsc::channel();
    let (hits_tx, hits_rx) = mpsc::channel();
    let shared = Arc::new(input::Shared::default());
    let store = ui::Store {
        runs: perf::runs_dir(),
        calibration: perf::calibration_path(),
    };
    let app = ui::App::new(rx, engine, bank, socket, soundfont, hits_rx, shared.clone())
        .with_store(store);
    let app = Arc::new(Mutex::new(app));
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Djembot")
            .with_inner_size([960.0, 720.0]),
        ..Default::default()
    };
    eframe::run_native(
        "djembot",
        options,
        Box::new(move |cc| {
            let ctx = cc.egui_ctx.clone();
            let served = app.clone();
            ipc::serve(
                listener,
                requests,
                move |deadline| {
                    // Past the deadline the request is expired, so a pass
                    // still holding the lock has nothing left to serve for it.
                    if let Some(mut app) = input::lock_until(&served, deadline) {
                        app.serve(Mono::now());
                    }
                    ctx.request_repaint();
                },
                ipc::REPLY_TIMEOUT,
            );
            ui::drive(app.clone()).context("starting the score thread")?;
            let ctx = cc.egui_ctx.clone();
            input::spawn(engine_hits, hits_tx, move || ctx.request_repaint(), shared)
                .context("starting the input thread")?;
            Ok(Box::new(ui::Window(app)))
        }),
    )
    .map_err(|e| {
        anyhow!(
            "could not open the window: {e}; run `djembot gui` from a terminal inside your \
             graphical session"
        )
    })
}

/// Validates the lesson, then sends it to the window with every `file`
/// reference already expanded.
fn show(input: &Input, play: Option<String>) -> ExitCode {
    let mut diag = Diagnostics::default();
    let lesson = input
        .read(&mut diag)
        .and_then(|text| lesson::parse(&text, input.dir(), &mut diag));
    report(input, &diag);
    let Some(lesson) = lesson else {
        return ExitCode::FAILURE;
    };
    let path = match input {
        Input::File(path) => std::path::absolute(path)
            .unwrap_or_else(|_| path.clone())
            .display()
            .to_string(),
        Input::Stdin => input.name(),
    };
    request(&ipc::Request::Show { path, lesson, play })
}

/// Sends one request to the window; prints its warnings and labels, or its
/// error.
fn request(req: &ipc::Request) -> ExitCode {
    let response = match send(req) {
        Ok(r) => r,
        Err(e) => {
            println!("error: {e}");
            return ExitCode::FAILURE;
        }
    };
    for line in response_lines(req, &response) {
        println!("{line}");
    }
    ExitCode::SUCCESS
}

/// What `request` prints for the window's successful `response` to `req`:
/// its warnings, the labels a show reports, then the armed block.
fn response_lines(req: &ipc::Request, response: &ipc::Response) -> Vec<String> {
    let mut out: Vec<String> = response
        .warnings
        .iter()
        .map(|w| format!("warning: {w}"))
        .collect();
    let labels = response.labels.as_deref().unwrap_or_default();
    match req {
        ipc::Request::Show { .. } if labels.is_empty() => {
            out.push("shown; no rhythm blocks".into())
        }
        ipc::Request::Show { .. } => out.push(format!("shown; blocks: {}", labels.join(", "))),
        ipc::Request::Calibrate => {
            out.push(format!("shown; blocks: {}", labels.join(", ")));
            out.push(CALIBRATE_HINT.into());
        }
        _ => {}
    }
    if let Some(label) = &response.armed {
        out.push(format!(
            "armed {label} — the user starts it with space (or ▶)"
        ));
    }
    out
}

/// What `djembot calibrate` tells the user to do.
const CALIBRATE_HINT: &str = "Tap `f` on every click once you have the beat; skips are fine. The \
     block stops by itself and the window shows the new offset; `djembot report` prints the run.";

/// Sends `wait` and prints every event line until a terminal one.
fn wait() -> ExitCode {
    let streamed =
        ipc::socket_path().and_then(|path| ipc::send_streaming(&path, &ipc::Request::Wait));
    let result = match streamed {
        Ok((response, _)) if !response.ok => Err(response
            .error
            .unwrap_or_else(|| "the window refused the request".into())),
        Ok((_, events)) => print_events(events, &mut io::stdout()),
        Err(e) => Err(format!("{e:#}")),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            println!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Writes each event line read from `events` to `out` as `wait` prints it,
/// up to and including a terminal one; an end of the stream before one is
/// the window closing.
fn print_events(events: impl BufRead, out: &mut impl Write) -> Result<(), String> {
    for line in events.lines() {
        let line = line.map_err(|e| format!("reading from the window: {e}"))?;
        let event: ipc::Event = serde_json::from_str(&line)
            .map_err(|e| format!("the window sent an unreadable event: {e}"))?;
        writeln!(out, "{}", event.line()).map_err(|e| e.to_string())?;
        if event.terminal {
            return Ok(());
        }
    }
    Err("the window closed".into())
}

/// The window's response to `req`; a refusal is the window's error text.
fn send(req: &ipc::Request) -> Result<ipc::Response, String> {
    match ipc::socket_path().and_then(|path| ipc::send(&path, req)) {
        Ok(r) if r.ok => Ok(r),
        Ok(r) => Err(r
            .error
            .unwrap_or_else(|| "the window refused the request".into())),
        Err(e) => Err(format!("{e:#}")),
    }
}

/// Prints the report (or with `raw` the JSON) of the run in `file`, else of
/// the window's open or last performance.
fn report_run(file: Option<&Path>, raw: bool) -> ExitCode {
    let got = match file {
        Some(path) => read_run(path).map(|run| (run, Some(path.display().to_string()))),
        None => send(&ipc::Request::Report).and_then(|r| {
            r.run
                .map(|run| (run, r.run_path))
                .ok_or_else(|| "the window sent no run".to_string())
        }),
    };
    match got {
        Ok((run, path)) => {
            if raw {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&run).expect("runs serialize")
                );
            } else {
                print!("{}", report::render(&run, path.as_deref().map(Path::new)));
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            println!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Reads a run file of the format this djembot writes or an earlier one.
/// The format version is checked before the rest, so a run from a later
/// version is refused by its version rather than by whichever field
/// changed.
fn read_run(path: &Path) -> Result<perf::Run, String> {
    let name = path.display();
    let text = std::fs::read_to_string(path).map_err(|e| format!("{name}: {e}"))?;
    let value: serde_json::Value =
        serde_json::from_str(&text).map_err(|e| format!("{name}: not a run file: {e}"))?;
    match value.get("djembot").and_then(serde_json::Value::as_u64) {
        Some(v) if (1..=u64::from(perf::RUN_FORMAT)).contains(&v) => {}
        Some(v) => {
            return Err(format!(
                "{name}: run format {v}; this djembot reads formats 1 to {}",
                perf::RUN_FORMAT
            ));
        }
        None => {
            return Err(format!(
                "{name}: not a run file: no \"djembot\" format version"
            ));
        }
    }
    serde_json::from_value(value).map_err(|e| format!("{name}: not a run file: {e}"))
}

fn report(input: &Input, diag: &Diagnostics) {
    let name = input.name();
    for w in &diag.warnings {
        println!("{name}: warning: {w}");
    }
    for e in &diag.errors {
        println!("{name}: error: {e}");
    }
}

/// Where a lesson or exercise comes from: a file, or `-` for stdin.
enum Input {
    File(PathBuf),
    Stdin,
}

impl Input {
    fn from(path: &Path) -> Input {
        if path == Path::new("-") {
            Input::Stdin
        } else {
            Input::File(path.to_path_buf())
        }
    }

    /// `<stdin>`, or the path as given.
    fn name(&self) -> String {
        match self {
            Input::File(path) => path.display().to_string(),
            Input::Stdin => "<stdin>".to_string(),
        }
    }

    /// The whole text; a read error is pushed to `diag`.
    fn read(&self, diag: &mut Diagnostics) -> Option<String> {
        let text = match self {
            Input::File(path) => std::fs::read_to_string(path),
            Input::Stdin => {
                let mut text = String::new();
                io::stdin().read_to_string(&mut text).map(|_| text)
            }
        };
        text.map_err(|e| diag.errors.push(e.to_string())).ok()
    }

    /// The directory relative `file` references resolve against: the
    /// file's directory, or the current directory for stdin.
    fn dir(&self) -> &Path {
        match self {
            Input::File(path) => path.parent().unwrap_or(Path::new(".")),
            Input::Stdin => Path::new("."),
        }
    }

    /// Whether the input is an exercise when `--exercise` is not given: a
    /// `.json` file is, anything else (stdin included) is a lesson.
    fn is_json(&self) -> bool {
        matches!(self, Input::File(path) if path.extension().is_some_and(|e| e == "json"))
    }
}

/// What a validated input contains.
enum Checked {
    Lesson(lesson::Lesson),
    Exercise(Box<Exercise>),
}

/// Validates a lesson or an exercise: an exercise when `exercise` is set
/// or the input is a `.json` file, a lesson otherwise.
fn check(input: &Input, exercise: bool, diag: &mut Diagnostics) -> Option<Checked> {
    let text = input.read(diag)?;
    check_text(&text, input.dir(), exercise || input.is_json(), diag)
}

/// Validates `text` as an exercise (`exercise`) or a lesson whose `file`
/// references resolve against `dir`.
fn check_text(text: &str, dir: &Path, exercise: bool, diag: &mut Diagnostics) -> Option<Checked> {
    if exercise {
        let value = lesson::parse_object(text, "", diag)?.into();
        Exercise::from_value(&value, 1, "", diag).map(|e| Checked::Exercise(Box::new(e)))
    } else {
        lesson::parse(text, dir, diag).map(Checked::Lesson)
    }
}

fn validate(input: &Input, exercise: bool) -> ExitCode {
    let mut diag = Diagnostics::default();
    let checked = check(input, exercise, &mut diag);
    report(input, &diag);
    let name = input.name();
    match checked {
        Some(Checked::Lesson(l)) => {
            let labels = l.labels();
            if labels.is_empty() {
                println!("{name}: ok, no rhythm blocks");
            } else {
                println!("{name}: ok, blocks: {}", labels.join(", "));
            }
            ExitCode::SUCCESS
        }
        Some(Checked::Exercise(e)) => {
            println!("{name}: ok, exercise \"{}\"", e.label);
            ExitCode::SUCCESS
        }
        None => ExitCode::FAILURE,
    }
}

#[cfg(test)]
mod library {
    //! Validating inputs, and CONTRIBUTING.md's heredoc lessons, which resolve
    //! against a learning directory (`starter/`).

    use super::*;

    /// Validates `text` written to a temporary `.json` file; returns the
    /// errors.
    fn exercise_file_errors(name: &str, text: &str) -> Vec<String> {
        let dir = std::env::temp_dir().join(format!("djembot-main-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("{name}.json"));
        std::fs::write(&path, text).unwrap();
        let mut diag = Diagnostics::default();
        assert!(check(&Input::File(path), false, &mut diag).is_none());
        diag.errors
    }

    #[test]
    fn invalid_exercise_files_report_without_a_block_prefix() {
        let errors = exercise_file_errors(
            "bad-voice",
            r#"{ "tempo": 90, "beats": 4, "voices": [
                { "name": "c", "instrument": "clave", "pulses": 0, "onsets": [], "colour": 1 } ] }"#,
        );
        assert_eq!(
            errors,
            vec![
                "voice 1: unknown field \"colour\" (known: name, instrument, pulses, onsets, \
                 gain, pan, mute, keys, silent)",
                "voice 1: pulses: must be an integer in 1..=1024, got 0",
            ]
        );
        assert_eq!(
            exercise_file_errors("array", "[1, 2]"),
            vec!["not a JSON object"]
        );
        let errors = exercise_file_errors("broken", "{ \"tempo\": ");
        assert_eq!(errors.len(), 1);
        assert!(errors[0].starts_with("invalid JSON: "), "{errors:?}");
    }

    #[test]
    fn check_text_picks_the_format() {
        let root = &Path::new(env!("CARGO_MANIFEST_DIR")).join("starter");
        let lesson = "# Stdin\n\n```djembot\n\
                      { \"label\": \"A\", \"file\": \"library/son-clave-3-2.json\" }\n```\n";
        let mut diag = Diagnostics::default();
        let Some(Checked::Lesson(l)) = check_text(lesson, root, false, &mut diag) else {
            panic!("not a lesson: {diag:?}");
        };
        assert_eq!(l.labels(), vec!["A"]);
        assert_eq!(l.blocks().next().unwrap().voices[0].pulses, 16);

        let mut diag = Diagnostics::default();
        assert!(check_text(lesson, root, true, &mut diag).is_none());
        assert_eq!(diag.errors.len(), 1);
        assert!(diag.errors[0].starts_with("invalid JSON: "), "{diag:?}");

        let exercise = std::fs::read_to_string(root.join("library/son-clave-3-2.json")).unwrap();
        let mut diag = Diagnostics::default();
        assert!(matches!(
            check_text(&exercise, Path::new("/nonexistent"), true, &mut diag),
            Some(Checked::Exercise(_))
        ));
        assert!(diag.errors.is_empty(), "{diag:?}");
    }

    #[test]
    fn inputs_name_and_resolve() {
        let stdin = Input::from(Path::new("-"));
        assert!(matches!(stdin, Input::Stdin));
        assert_eq!(stdin.name(), "<stdin>");
        assert_eq!(stdin.dir(), Path::new("."));
        assert!(!stdin.is_json());

        let file = Input::from(Path::new("lessons/x.md"));
        assert_eq!(file.name(), "lessons/x.md");
        assert_eq!(file.dir(), Path::new("lessons"));
        assert!(!file.is_json());
        assert!(Input::from(Path::new("library/x.json")).is_json());

        let mut diag = Diagnostics::default();
        assert!(
            check(
                &Input::from(Path::new("/nonexistent/x.md")),
                false,
                &mut diag
            )
            .is_none()
        );
        assert_eq!(diag.errors.len(), 1, "{diag:?}");
        let io = std::fs::read_to_string("/nonexistent/x.md").unwrap_err();
        assert_eq!(diag.errors[0], io.to_string(), "the name is not repeated");
    }

    #[test]
    fn exercise_flag_overrides_the_extension() {
        let path =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("starter/lessons/000-twelve-vs-sixteen.md");
        let mut diag = Diagnostics::default();
        assert!(check(&Input::File(path), true, &mut diag).is_none());
        assert!(diag.errors[0].starts_with("invalid JSON: "), "{diag:?}");
    }

    /// Every `<<'EOF'` heredoc in CONTRIBUTING.md and in the instructor
    /// contract is a stdin lesson that validates from a learning directory,
    /// with at least one rhythm block.
    #[test]
    fn heredoc_lessons_validate() {
        let root = &Path::new(env!("CARGO_MANIFEST_DIR")).join("starter");
        for doc in ["../CONTRIBUTING.md", "instructor.md"] {
            let text = std::fs::read_to_string(root.join(doc)).unwrap();
            let mut lines = text.lines();
            let mut found = 0;
            while let Some(line) = lines.by_ref().find(|l| l.trim_end().ends_with("<<'EOF'")) {
                let indent = line.len() - line.trim_start().len();
                let body: Vec<&str> = lines
                    .by_ref()
                    .take_while(|l| l.trim() != "EOF")
                    .map(|l| l.get(indent..).unwrap_or(l.trim_start()))
                    .collect();
                let text = body.join("\n") + "\n";
                let mut diag = Diagnostics::default();
                let checked = check_text(&text, root, false, &mut diag);
                assert!(
                    diag.errors.is_empty() && diag.warnings.is_empty(),
                    "{doc}: {line}: {diag:?}"
                );
                let Some(Checked::Lesson(l)) = checked else {
                    panic!("{doc}: {line}: not a lesson");
                };
                assert!(
                    l.blocks().next().is_some(),
                    "{doc}: {line}: no rhythm blocks"
                );
                found += 1;
            }
            assert!(found >= 1, "{doc} has no heredoc lessons");
        }
    }
}

#[cfg(test)]
mod calibrate {
    //! What `djembot calibrate` tells the user matches the calibration lesson.

    use super::*;
    use clap::CommandFactory;

    #[test]
    fn the_hint_and_help_name_the_calibration_key_and_procedure() {
        let mut diag = Diagnostics::default();
        let lesson = lesson::parse(perf::CALIBRATION_LESSON, Path::new("/"), &mut diag).unwrap();
        let blocks: Vec<&Exercise> = lesson.blocks().collect();
        assert_eq!(blocks.len(), 1, "calibrate plays block 1, its only block");
        let voices: Vec<usize> = blocks[0].player_voices().collect();
        assert_eq!(voices.len(), 1);
        let keys = &blocks[0].voices[voices[0]].keys;
        assert_eq!(keys.len(), 1);
        let key = format!("`{}`", keys[0].key);
        assert!(
            CALIBRATE_HINT.contains(&format!("Tap {key} on every click once you have the beat")),
            "{CALIBRATE_HINT}"
        );
        assert_eq!(blocks[0].count_in, 0, "the user comes in when ready");
        assert_eq!(blocks[0].cycles, None, "the window stops the block");
        assert!(
            CALIBRATE_HINT.contains("skips are fine") && CALIBRATE_HINT.contains("stops by itself"),
            "{CALIBRATE_HINT}"
        );
        let help = Cli::command()
            .find_subcommand("calibrate")
            .and_then(|c| c.get_about().map(|a| a.to_string()))
            .unwrap();
        assert!(help.contains(&format!("tap {key}")), "{help}");
    }
}

#[cfg(test)]
mod response {
    //! What a command prints from the window's response.

    use super::*;

    #[test]
    fn wait_prints_events_until_a_terminal_one() {
        let stream = [
            r#"{"event": "playing", "label": "guided", "cycle": 3, "cycles": 8}"#,
            r#"{"event": "started", "label": "alone"}"#,
            &r#"{"event": "ended", "label": "alone", "by": "stop", "cycles_played": 5,
                "cycles": 8, "hit": 18, "miss": 2, "extra": 1, "run": "/r/a.json",
                "terminal": true}"#
                .replace('\n', ""),
            r#"{"event": "started", "label": "after"}"#,
        ]
        .join("\n");
        let mut out = Vec::new();
        assert_eq!(print_events(stream.as_bytes(), &mut out), Ok(()));
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "playing label=guided cycle=3/8\nstarted label=alone\nended label=alone by=stop \
             cycles=5/8 hit=18 miss=2 extra=1 run=/r/a.json\n"
        );
        let mut out = Vec::new();
        let open = r#"{"event": "idle"}"#;
        assert_eq!(
            print_events(open.as_bytes(), &mut out),
            Err("the window closed".into())
        );
        assert_eq!(out, b"idle\n");
    }

    #[test]
    fn play_prints_the_armed_line() {
        let play = ipc::Request::Play {
            label: Some("A".into()),
        };
        let armed = ipc::Response {
            armed: Some("A".into()),
            ..ipc::Response::ok(vec!["A".into(), "B".into()])
        };
        assert_eq!(
            response_lines(&play, &armed),
            ["armed A — the user starts it with space (or ▶)"]
        );
        let started = ipc::Response::ok(vec!["A".into()]);
        assert!(response_lines(&play, &started).is_empty());

        let show = ipc::Request::Show {
            path: "/l.md".into(),
            lesson: lesson::Lesson { segments: vec![] },
            play: Some("A".into()),
        };
        let armed = ipc::Response {
            warnings: vec!["no keyboard".into()],
            ..armed
        };
        assert_eq!(
            response_lines(&show, &armed),
            [
                "warning: no keyboard",
                "shown; blocks: A, B",
                "armed A — the user starts it with space (or ▶)",
            ]
        );
    }
}
