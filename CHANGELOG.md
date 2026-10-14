# Changelog

Notable changes to djembot. Format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versions follow
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Changed

- **The journal is the student's current state; sessions go to `log/`.**
  `JOURNAL.md` is rewritten at the end of every session — the student,
  where they are, tool notes — instead of growing by one session's notes
  each time, and its *Latest session* points at the session in progress.
  Each session gets `log/YYYY/MM/DD-NN/`, created when it starts, holding
  `session.md` (written as the session goes) and the session's run files,
  each copied in as `djembot wait` reports it. The instructor reads only the
  journal and the session it names, so startup reads the same amount
  however long the history is. `init` lays down nothing new; the first
  session creates `log/`.

  An existing learning directory updates by hand: delete its `CLAUDE.md`
  and run `djembot init` again, which writes the new one and keeps every
  other file (`init` never overwrites, and reports a used journal as
  differing from the template whichever layout it has); then rewrite
  `JOURNAL.md` to the new template's shape — its opening paragraph and
  its four sections — keeping *The student*, *Where we are* and *Tool
  notes* as they are, moving the *Sessions* text into a first `log/`
  entry (`log/YYYY/MM/DD-01/session.md`, dated when you do it) and
  naming that file under *Latest session*. A journal that was never
  written to is deleted and rewritten by `init` the same way.

- **A late tap on the closing "one" is now judged instead of dropped.**
  After the last block of a performance ends, the window keeps judging
  strokes for one beat (or the timing window, if that is longer) — the
  same allowance it already gave across a block boundary. Before, a
  closing tap more than one timing window late vanished, so the report
  showed a plain miss and the instructor could not tell "never tapped the
  last one" from "tapped it late". Now such a tap appears as an extra
  stroke, counted *wide* when it was nearest the missed closing one. The
  standard for a hit is unchanged. As a consequence, a finished
  performance ends — `ended` fires, `wait` returns, the run file is
  written — up to a beat later than before, and a restart or stop within
  that beat is recorded as a restart or stop rather than a completed run.
- **The instructor guide now says in what order `wait` reports a block
  change.** `started` is printed when the music reaches a block; `ended`
  when nothing is left to judge in it. So when a block with a part plays
  straight into the next, `started` for the new block comes before
  `ended` for the old one; after a block with no part it is the other way
  round. The output itself is unchanged. The instructor guide is the
  `CLAUDE.md` that `djembot init` writes, so only new learning
  directories get the new text.

### Fixed

- **The test suite passes regardless of the machine's uptime.** Several
  window tests replay a simulated session but asked the real clock what
  was playing, so they failed on hosts that had been up for a long time,
  GitHub's CI among them. They now ask about the simulated moment
  instead. No change to the program's behaviour.

## [0.1.0] — 2026-10-13

First public release. Djembot is a rhythm trainer for learning Afro-Cuban
and West African rhythm by ear: an LLM instructor writes markdown lessons
with embedded rhythm blocks, a window on the learner's screen plays them and
shows them in sync, and the learner can play a part on the computer keyboard
while the window judges each stroke and writes a report the instructor
teaches from. Linux only.

### Getting started

- **Install with `cargo install --locked djembot`.** The README lists the
  system packages it needs per distribution (Fedora, Debian/Ubuntu, Arch):
  FluidSynth, a General MIDI soundfont and the ALSA headers.
- **`djembot init [DIR]` sets up a learning directory** — no need to clone
  this repository. It writes the instructor's instructions (`CLAUDE.md`), a
  progress journal (`JOURNAL.md`), a library of standard rhythms
  (`library/`) and a first lesson (`lessons/`). Files that already exist are
  kept and reported, never overwritten. Point Claude Code (or another coding
  agent, via `ln -s CLAUDE.md AGENTS.md`) at that directory and it becomes
  your instructor.

