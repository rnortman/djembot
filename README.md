# Djembot

A rhythm trainer for learning by ear. An LLM instructor (Claude Code, or
another coding agent) writes short markdown lessons with embedded rhythm
blocks; a window beside the terminal shows each block as a grid and a
necklace and plays it, with the playhead and the ring's hand driven by the
audio clock so what you see is what you hear.

A lesson can also give you a part to play on the computer keyboard: each
bound key sounds the part's instrument, the window marks every stroke as
early, on time, late, missed or extra as you play, and the instructor reads
a report of the run (`djembot report`).

Linux only (Wayland or X11): the window reads the keyboard through evdev.
There is no macOS or Windows support. Licensed under Apache-2.0.

## Install

The system packages — FluidSynth to build against, a General MIDI
soundfont to render the percussion from, and ALSA headers:

```sh
# Fedora
sudo dnf install fluidsynth-devel fluid-soundfont-gm alsa-lib-devel
# Debian, Ubuntu
sudo apt install libfluidsynth-dev fluid-soundfont-gm libasound2-dev pkg-config
# Arch
sudo pacman -S fluidsynth soundfont-fluid alsa-lib pkgconf
```

Check: `pkg-config --modversion fluidsynth` prints `2.x`, and the
soundfont exists — `/usr/share/soundfonts/default.sf2` (Fedora),
`/usr/share/sounds/sf2/FluidR3_GM.sf2` (Debian, Ubuntu) or
`/usr/share/soundfonts/FluidR3_GM.sf2` (Arch). You also need a graphical
session and an audio output (PipeWire or PulseAudio through ALSA).

Then a Rust toolchain ([rustup](https://rustup.rs)) and:

```sh
cargo install --locked djembot
```

## Start learning

```sh
mkdir rhythm && cd rhythm
djembot init          # CLAUDE.md for your instructor, a journal, the library, a first lesson
git init && git add -A && git commit -m "Start"
djembot gui           # in a terminal inside your graphical session; keep it open
claude                # in another terminal, in this directory
```

The instructor reads `CLAUDE.md`, writes lessons into `lessons/`, drives
the window with the commands below, and keeps `JOURNAL.md` so the next
session picks up where this one stopped; the directory is yours, and so is
its history. For an agent that reads `AGENTS.md` instead, `ln -s CLAUDE.md
AGENTS.md`. Before playing a part, join the `input` group (*Keyboard
access*) and calibrate once (`djembot calibrate`).

The agent's shell cannot open the window itself; it only runs the other
commands, which talk to the window over a socket.

## Commands

| command | what it does |
|---|---|
| `djembot gui [--soundfont PATH]` | the window; runs until closed |
| `djembot show LESSON.md\|- [--play LABEL]` | validate the lesson; print every error (exit 1), or show it in the window and print its block labels; `--play` as `djembot play` |
| `djembot play [LABEL]` | play a block (label or 1-based number; default the first) from its count-in, on into the blocks it is chained to with `"next": "attacca"` (its score); a score with a part (`keys`) in any of its blocks is armed instead: it waits, `press space to start`, for the user's `space` or ▶ |
| `djembot stop` | stop playback |
| `djembot wait` | print the window's state, then each transport event (`shown`, `armed`, `started`, `ended`, one line each, `ended` with its run file); exit once the performance is over (stopped, or played to the end of its score); `error: the window closed` (exit 1) if the window goes first |
| `djembot report [RUN.json] [--raw]` | print the report of the open or last performance, or of a saved run file (no window needed then); `--raw` prints the run JSON |
| `djembot calibrate` | show and play the calibration block: the clicks start at once; tap `f` on every click once you have the beat (the taps make no sound, skips are fine); the block stops by itself after 24 taps and the window stores the measured offset |
| `djembot init [DIR]` | lay the instructor contract (`CLAUDE.md`), a journal (`JOURNAL.md`), the library (`library/`) and a first lesson (`lessons/`) into `DIR` (default: here; created if absent); prints `wrote PATH` per file, and keeps any file that exists (`kept PATH (same as this version)` or `(differs from this version's template)`) |
| `djembot validate FILE\|- [--exercise]` | check a lesson (`.md`) or an exercise (`.json`, or any input with `--exercise`); no window needed |


A path of `-` reads the lesson (or, with `--exercise`, the exercise) from
stdin; its `file` references resolve against the current directory, and it
is named `<stdin>` in messages and in the window's status line.

In the window: ▶/■ on each block, click a row label to mute that voice.
The outlined block is the selected one, and the keys act on it: `↓`/`j`
next block, `↑`/`k` previous block (selecting does not play), `space`
play/pause (and start an armed block), `r` restart from the count-in, `h` hide/reveal its grid, `1`–`9`
mute voice N, `q` quit. Playing a block selects it. The status line shows the lesson, what is playing (`playing A ·
cycle 3/8`, or `armed A`), the soundfont, the keyboard status (`keys: 1 device`), the
calibration (`calibration: +12 ms` or `uncalibrated`) and, after a run is
saved, its path.

## Keyboard access

The window reads the keyboard from `/dev/input/event*` directly, for the
kernel's timestamp of each key press. Those files are `root:input 0660`, so
you must be in the `input` group:

```sh
sudo usermod -aG input $USER    # then log out and in again
ls -l /dev/input/event0          # crw-rw----. 1 root input …
id -nG | grep -w input           # prints your groups when you are in
```

(Or a udev rule giving your user access to the keyboard's event device.)
Until then the status line reads `keys: no access to /dev/input/eventN — …`,
`djembot show` of a lesson with a part warns, and a part records only
misses. The window rescans every 2 s, so it picks the keyboard up without a
restart. Strokes count only while the window has focus: typing in another
window neither sounds nor scores. Laptop and external keyboards are both
read; the same key on either plays the same part.

## Latency

The window asks the device for a 128-frame period (2.7 ms at 48 kHz). The
status line's `audio:` segment shows what it got: the frames per callback
and the output latency the device reports, e.g. `audio: 128 frames · 9 ms`.
A device that ignores the request shows a larger period and a
correspondingly larger latency; the quantum setting below is for that.

That latency is subtracted from every stroke before it is judged: a stroke
is compared with the click that was leaving the speakers when the key went
down. `djembot report` prints it in its `latency` row. If the status line
shows a latency well above the period, try a smaller PipeWire quantum:

```sh
PIPEWIRE_LATENCY=128/48000 djembot gui
```

What is left is what `djembot calibrate` measures; its usual source is the
keyboard's actuation point (how far down a key registers) and tapping
ahead of a click, confirmed by the tempo check the instructor contract
describes (`starter/instructor.md`, *Calibration*; `CLAUDE.md` in a
learning directory). The calibration procedure: the
clicks start at once, you tap `f` on every click once you have the beat
(the taps make no sound, and skipped clicks do not count), and the block
stops after 24 taps; the median error becomes the offset subtracted from
every later stroke. Stopping it yourself after at least 16 taps keeps the
offset too. Calibrate once per machine, keyboard and output device.

## Files

- Runs: `$XDG_DATA_HOME/djembot/runs/YYYYMMDD-HHMMSS-LABEL.json`
  (`~/.local/share/djembot/runs/`), one per performance with at least one
  stroke or miss, written when it ends; a score writes one per block with
  a part. `djembot report RUN.json` prints the report of one, and `djembot
  wait` prints each one's path in its `ended … run=PATH` line.
- Calibration: `$XDG_CONFIG_HOME/djembot/calibration.json`
  (`~/.config/djembot/calibration.json`), read when `djembot gui` starts.

## Soundfont

The first of these that is *specified* is used, and that choice is final:

1. `--soundfont PATH` — must be an existing file, otherwise `djembot gui`
   exits with `--soundfont PATH: no such file`.
2. Else `$DJEMBOT_SOUNDFONT`, if set and non-empty — same rule; a missing
   file exits with `DJEMBOT_SOUNDFONT=VALUE: no such file` (fix it, or
   `unset DJEMBOT_SOUNDFONT` to use the default).
3. Else `/usr/share/soundfonts/default.sf2` (Fedora),
   `/usr/share/soundfonts/FluidR3_GM.sf2` (Fedora, Arch), then
   `/usr/share/sounds/sf2/FluidR3_GM.sf2` (Debian, Ubuntu); if none exists
   `djembot gui` exits naming `fluid-soundfont-gm`, the paths it tried,
   and `--soundfont PATH`.

## Socket

The window listens on `$XDG_RUNTIME_DIR/djembot.sock`. If `XDG_RUNTIME_DIR`
is unset or empty, it uses `/tmp/djembot-<uid>/djembot.sock` instead: that
directory is created with mode 0700, and `djembot gui` and every client
refuse it unless it is a real directory (not a symlink) owned by you with
mode 0700 — remove it, or set `XDG_RUNTIME_DIR`.

If the window is busy, a command waits up to 6 s and then prints `error: gui
did not respond`. A request the window has not reached 5 s after it was sent
is dropped without being acted on, so a late `show`, `play` or `stop` never
changes the window after the command has already reported failure. A
request is answered whether or not the compositor is drawing the window
(hidden, covered, on another workspace); the display catches up when the
window is shown.

Only one `djembot gui` runs at a time; a second one exits with "another
djembot gui is running". A socket file left by a crashed window is replaced
on the next start.

## Contributing

Contributions are welcome.
`CONTRIBUTING.md` has the build, the checks and the manual test of the
window.
