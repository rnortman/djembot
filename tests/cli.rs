//! The `djembot` binary without a window: reading lessons and exercises
//! from stdin, and reports from run files. `validate` and `report RUN.json`
//! need no window; `show` and `report` are pointed at an empty runtime
//! directory where nothing listens.

use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};

const LESSON: &str = "# From stdin\n\n```djembot\n\
                      { \"label\": \"A\", \"file\": \"starter/library/son-clave-3-2.json\", \"cycles\": 2 }\n\
                      ```\n";

const EXERCISE: &str = r#"{ "label": "P", "tempo": 90, "beats": 4, "voices": [
    { "name": "pulse", "instrument": "click", "pulses": 4, "onsets": [0, 1, 2, 3] } ] }"#;

/// Runs `djembot args…` from the repository root with `stdin` piped in.
fn run(args: &[&str], stdin: &str, runtime_dir: Option<&PathBuf>) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_djembot"));
    cmd.args(args)
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(dir) = runtime_dir {
        cmd.env("XDG_RUNTIME_DIR", dir);
    }
    let mut child = cmd.spawn().unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(stdin.as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
}

fn stdout(out: &Output) -> String {
    String::from_utf8(out.stdout.clone()).unwrap()
}

#[test]
fn validate_reads_a_lesson_from_stdin() {
    let out = run(&["validate", "-"], LESSON, None);
    assert_eq!(stdout(&out), "<stdin>: ok, blocks: A\n");
    assert!(out.status.success());
}

#[test]
fn validate_reports_stdin_errors_by_name() {
    let bad = LESSON.replace(
        "\"cycles\": 2 }",
        "\"voices\": [ { \"name\": \"c\", \"instrument\": \"clave\", \"pulses\": 16, \
         \"onsets\": [16] } ] }",
    );
    let out = run(&["validate", "-"], &bad, None);
    let text = stdout(&out);
    assert!(
        text.starts_with("<stdin>: error: block 1 \"A\": voice 1: onsets["),
        "{text}"
    );
    assert_eq!(out.status.code(), Some(1));
}

#[test]
fn validate_exercise_flag_reads_an_exercise_from_stdin() {
    let out = run(&["validate", "--exercise", "-"], EXERCISE, None);
    assert_eq!(stdout(&out), "<stdin>: ok, exercise \"P\"\n");
    assert!(out.status.success());

    // Without the flag stdin is a lesson, and JSON without a fence is text.
    let out = run(&["validate", "-"], EXERCISE, None);
    assert_eq!(stdout(&out), "<stdin>: ok, no rhythm blocks\n");
}

#[test]
fn show_validates_stdin_then_tries_the_window() {
    let dir = std::env::temp_dir().join(format!("djembot-cli-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let out = run(&["show", "-", "--play", "A"], LESSON, Some(&dir));
    let text = stdout(&out);
    assert!(
        text.starts_with("error: no `djembot gui` is listening at "),
        "{text}"
    );
    assert_eq!(out.status.code(), Some(1));
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn validate_names_the_voice_with_bad_keys() {
    let bad = LESSON.replace("\"cycles\": 2 }", "\"cycles\": 2, \"voices\": [ { \"name\": \"c\", \"instrument\": \"clave\", \"pulses\": 16, \"onsets\": [2], \"keys\": \"space\" } ] }");
    let out = run(&["validate", "-"], &bad, None);
    let text = stdout(&out);
    assert!(
        text.starts_with("<stdin>: error: block 1 \"A\": voice 1: keys: unknown key \"space\""),
        "{text}"
    );
    assert_eq!(out.status.code(), Some(1));
}

const RUN: &str = r#"{
  "djembot": 1, "lesson": "/abs/lessons/004-clave-on-f.md", "label": "A",
  "title": "Son clave", "tempo": 90.0, "beats": 4, "cycles": 1, "count_in": 4,
  "window_ms": 100.0, "offset_ms": 0.0, "input": "1 device",
  "started": "2026-10-11T10:12:03Z", "ended": "2026-10-11T10:12:31Z",
  "ended_by": "end", "cycles_played": 1,
  "parts": [ { "voice": 1, "name": "clave", "instrument": "clave",
               "keys": {"x": "f"}, "pulses": 16, "onsets": [2, 4] } ],
  "hits": [ { "t_ms": 319.2, "voice": 1, "tone": "x", "cycle": 0, "pulse": 2,
              "target_tone": "x", "error_ms": -14.2, "judgment": "hit" } ],
  "misses": [ { "cycle": 0, "voice": 1, "pulse": 4 } ]
}"#;

/// A fresh, empty directory for one test.
fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("djembot-cli-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn report_prints_a_run_file_without_a_window() {
    let dir = scratch("report");
    let path = dir.join("20261011-101203-A.json");
    std::fs::write(&path, RUN).unwrap();
    let file = path.to_str().unwrap();

    let out = run(&["report", file], "", Some(&dir));
    let text = stdout(&out);
    assert!(out.status.success(), "{text}");
    assert!(
        text.starts_with("lesson   /abs/lessons/004-clave-on-f.md\n"),
        "{text}"
    );
    assert!(text.contains(&format!("\nrun      {file}\n")), "{text}");
    assert!(text.contains("\nPer note\n"), "{text}");
    assert!(
        text.contains("\nlatency  –\n"),
        "a format-1 run has no latency: {text}"
    );

    let out = run(&["report", "--raw", file], "", Some(&dir));
    let raw: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    // An older file is printed as read: it recorded no closing stroke.
    let mut want = serde_json::from_str::<serde_json::Value>(RUN).unwrap();
    want["ending"] = "none".into();
    assert_eq!(raw, want);

    std::fs::write(&path, "{}").unwrap();
    let out = run(&["report", file], "", Some(&dir));
    assert!(
        stdout(&out).starts_with(&format!("error: {file}: not a run file: ")),
        "{}",
        stdout(&out)
    );
    assert_eq!(out.status.code(), Some(1));

    let format_2 = RUN
        .replace(
            "\"djembot\": 1,",
            "\"djembot\": 2, \"latency_ms\": 9.0, \"ended_by\": \"enough\",",
        )
        .replace("\"ended_by\": \"end\",", "");
    std::fs::write(&path, format_2).unwrap();
    let out = run(&["report", file], "", Some(&dir));
    let text = stdout(&out);
    assert!(out.status.success(), "{text}");
    assert!(
        text.contains("\nlatency  9.0 ms (output, device-reported)\n"),
        "{text}"
    );
    assert!(text.contains("\nended    enough ("), "{text}");

    let format_3 = RUN.replace(
        "\"djembot\": 1,",
        "\"djembot\": 3, \"ending\": \"downbeat\",",
    );
    std::fs::write(&path, format_3).unwrap();
    let out = run(&["report", file], "", Some(&dir));
    let text = stdout(&out);
    assert!(out.status.success(), "{text}");
    assert!(
        text.contains("\ncycles   1 of 1 cycles played, ending on the downbeat\n"),
        "{text}"
    );

    std::fs::write(&path, RUN.replace("\"djembot\": 1,", "\"djembot\": 4,")).unwrap();
    let out = run(&["report", file], "", Some(&dir));
    assert_eq!(
        stdout(&out),
        format!("error: {file}: run format 4; this djembot reads formats 1 to 3\n")
    );
    assert_eq!(out.status.code(), Some(1));
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn report_without_a_file_asks_the_window() {
    let dir = scratch("report-window");
    let out = run(&["report"], "", Some(&dir));
    let text = stdout(&out);
    assert!(
        text.starts_with("error: no `djembot gui` is listening at "),
        "{text}"
    );
    assert_eq!(out.status.code(), Some(1));
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn wait_without_a_window_errors() {
    let dir = scratch("wait");
    let out = run(&["wait"], "", Some(&dir));
    let text = stdout(&out);
    assert!(
        text.starts_with("error: no `djembot gui` is listening at "),
        "{text}"
    );
    assert_eq!(out.status.code(), Some(1));
    std::fs::remove_dir_all(&dir).unwrap();
}

/// `starter/` on disk as (target path, content), as `djembot init` lays it
/// down: `instructor.md` becomes `CLAUDE.md`.
fn starter_files() -> Vec<(String, String)> {
    fn walk(dir: &std::path::Path, out: &mut Vec<PathBuf>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                walk(&path, out);
            } else {
                out.push(path);
            }
        }
    }
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("starter");
    let mut paths = Vec::new();
    walk(&root, &mut paths);
    let mut files: Vec<(String, String)> = paths
        .into_iter()
        .map(|p| {
            let rel = p.strip_prefix(&root).unwrap().to_str().unwrap();
            let target = if rel == "instructor.md" {
                "CLAUDE.md"
            } else {
                rel
            };
            (target.to_string(), std::fs::read_to_string(&p).unwrap())
        })
        .collect();
    files.sort();
    files
}

/// The `wrote`/`kept` lines of an `init` run, sorted, without the `next:` line.
fn init_lines(out: &Output) -> Vec<String> {
    let text = stdout(out);
    let mut lines: Vec<String> = text.lines().map(str::to_string).collect();
    assert!(
        lines
            .pop()
            .is_some_and(|l| l.starts_with("next: git init here")),
        "{text}"
    );
    lines.sort();
    lines
}

#[test]
fn init_writes_the_kit_then_keeps_it() {
    let dir = scratch("init");
    let target = dir.join("rhythm/new");
    let target_arg = target.to_str().unwrap();
    let files = starter_files();
    assert!(files.iter().any(|(t, _)| t == "CLAUDE.md"));

    let out = run(&["init", target_arg], "", None);
    assert!(out.status.success());
    let wrote: Vec<String> = files.iter().map(|(t, _)| format!("wrote {t}")).collect();
    assert_eq!(init_lines(&out), wrote);
    for (t, content) in &files {
        assert_eq!(
            &std::fs::read_to_string(target.join(t)).unwrap(),
            content,
            "{t}"
        );
    }

    let out = run(&["init", target_arg], "", None);
    assert!(out.status.success());
    let same: Vec<String> = files
        .iter()
        .map(|(t, _)| format!("kept {t} (same as this version)"))
        .collect();
    assert_eq!(init_lines(&out), same);

    std::fs::write(target.join("CLAUDE.md"), "my own notes\n").unwrap();
    let out = run(&["init", target_arg], "", None);
    assert!(out.status.success());
    let lines = init_lines(&out);
    assert!(
        lines.contains(&"kept CLAUDE.md (differs from this version's template)".to_string()),
        "{lines:?}"
    );
    assert_eq!(
        std::fs::read_to_string(target.join("CLAUDE.md")).unwrap(),
        "my own notes\n"
    );
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn init_that_cannot_write_prints_the_error_and_exits_1() {
    let dir = scratch("init-error");
    std::fs::create_dir_all(dir.join("CLAUDE.md")).unwrap();
    let out = run(&["init", dir.to_str().unwrap()], "", None);
    let text = stdout(&out);
    assert!(
        text.starts_with(&format!("error: {}: ", dir.join("CLAUDE.md").display())),
        "{text}"
    );
    assert_eq!(out.status.code(), Some(1));
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn init_without_a_directory_writes_into_the_current_one() {
    let dir = scratch("init-here");
    let out = Command::new(env!("CARGO_BIN_EXE_djembot"))
        .arg("init")
        .current_dir(&dir)
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stdout(&out));
    for (t, content) in &starter_files() {
        assert_eq!(
            &std::fs::read_to_string(dir.join(t)).unwrap(),
            content,
            "{t}"
        );
    }
    std::fs::remove_dir_all(&dir).unwrap();
}
