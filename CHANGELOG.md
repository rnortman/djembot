# Changelog

Notable changes to djembot. Format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versions follow
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Changed

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

