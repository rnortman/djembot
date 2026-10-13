# Djembot

A rhythm trainer for learning by ear: an LLM instructor writes markdown
lessons with embedded rhythm blocks, and drives a window that plays and
shows them through the `djembot` CLI. The CLI talks to the window over a
Unix socket (`ipc.rs`); the window is an egui UI (`ui.rs`), an audio engine
(`engine.rs`, `mixer.rs`, `fluid.rs`, `sounds.rs`), evdev keyboard input
(`input/`), and judging and run files (`perf.rs`, `report.rs`). Lessons and
exercises are parsed and validated in `lesson.rs` and `model.rs`, and
scheduled in `timeline.rs`.

## Building

```
make check                        — full pre-commit suite (msrv, fmt, clippy, test, scrub kit)
cargo test -- --include-ignored   — also the real soundfont and audio device
```

The system packages the build and the tests need are listed in the README
(*Install*, per distribution).

## The two specs

`starter/instructor.md` is the user-facing spec of the CLI and the lesson
format, written for the instructor; the README's command table is the
other. A change to a command's output, a validation rule, a key or a
message updates both in the same change. `starter/` is embedded in the
binary (`src/starter.rs`), and `djembot init` lays it into a learner's
directory, `instructor.md` as `CLAUDE.md`; a file added under `starter/`
needs its `FILES` entry, or `make check` fails.

## Principles

- **Every command prints `error: …` and exits 1 on failure.**
- **The window never guesses.** A request it did not reach in time is
  dropped, not applied late (`gui did not respond`).
- **Onsets are pulse indices**, never grid strings.
- **No compatibility shims** for old formats, names or paths: an old run
  file is rejected, not converted.
- **A file named explicitly must exist**, and nothing else is tried for it
  (`--soundfont`, `DJEMBOT_SOUNDFONT`, a lesson's `file`). The deliberate
  defaults — the socket's `/tmp` directory without `XDG_RUNTIME_DIR`, XDG
  directories falling back to `$HOME`, the device's buffer size, the
  soundfont search — are defaults, not fallbacks from a named choice.
- **Resist disabling lint checks.** Fix clippy warnings, don't suppress
  them.
- **Always write unit tests.**

## TODOs

Track deferred work in `TODO.md` (created by the first deferral, headed as
a list of deferred work joined by slug to the code). Each entry gets a
kebab-case slug as its heading (`## \`my-slug\``) and is referenced from
code with `// TODO(my-slug)`. When completing a TODO, remove both the entry
and every `TODO(slug)` comment. The file ships publicly: no private paths,
no workflow narration.

## Code Standards

- Rust edition 2024, toolchain pinned in `rust-toolchain.toml`.
- `cargo fmt`, `cargo clippy -D warnings` and `cargo test` must pass.
- Linux only.
- The pre-commit hook runs the scrub gate and `make check`.
- Default branch: `main`.
