# Contributing

## Building

```sh
git clone https://github.com/rnortman/djembot && cd djembot
cargo install --locked brenn-scrub
brenn-scrub init                  # points git at .githooks, the scrub gate
make check                        # msrv, fmt, clippy, tests, scrub kit
cargo test -- --include-ignored   # also the real soundfont and audio device
```

The same system packages as the README's *Install*; `cargo test` needs
no audio, display or soundfont. The commit and push hooks run
[brenn-scrub](https://github.com/rnortman/brenn-scrub), which needs
[gitleaks 8.30.0](https://github.com/gitleaks/gitleaks/releases/tag/v8.30.0)
on `PATH`; `make check` fails until `brenn-scrub init` has run in the
clone, and `make rust-check` is the Rust part alone. The
layout: `src/` (the binary), `tests/` (the CLI without a window),
`starter/` (the learner kit, embedded in the binary and laid down by
`djembot init`; `starter/instructor.md` is the instructor contract).
`CLAUDE.md` is the developer contract.

## Manual check

Run from a clone, with `djembot` built from it
(`cargo install --locked --path .`), in a terminal inside your graphical
session:

```sh
djembot gui
```

In another terminal, from the repository root:

```sh
djembot show starter/lessons/000-twelve-vs-sixteen.md --play 3
```

Confirm that:

- audio starts after the four-beat count-in, which counts up
  (`1 · 2 · 3 · 4`) in the block header;
- the playhead on the grid and the hand on the ring move in sync with the
  sound, and each stroke flashes as it plays;
- the clave and the bell line up on the beat lines;
- clicking a row label, or pressing `1` / `2`, mutes that voice;
- `djembot play 1` switches to the first block and scrolls to it;
- the selected block is outlined; `j` moves the outline to the next block
  without starting it; `space` then plays it;
- a block with `"hidden": true` shows no grid until `h`;
- `djembot stop` stops;
- `djembot show - <<'EOF'` with a one-block lesson, then `EOF`, shows a
  lesson named `<stdin>`;
- `djembot show` of a lesson with a typo in an onset (say `"pulses": 16`
  with an onset of `16`) prints the block, voice and field, and the window
  is unchanged.

Playing a part (after joining the `input` group, see the README's
*Keyboard access*):

1. `djembot gui`; the status line reads `keys: N devices`, `audio: 128
   frames · N ms`, the latency the device reports, and `uncalibrated`.
2. `djembot calibrate`: the clicks start at once and the taps make no
   sound. Come in after a few clicks and skip two on purpose; the header
   counts `taps N/24 · median ±NN ms`, and the block stops by itself,
   after the 24th click is heard. The status line shows
   `calibration: ±NN ms`; `djembot report` shows `ended enough`, 24 hits,
   the clicks before you came in and the skipped ones as misses, the
   `latency` row, and the stored offset as the `median` of its Overall
   row (its `offset` row reads `+0.0`: the calibration block is the one
   block judged without an offset).
3. Run the tempo check as `starter/instructor.md` *Calibration* describes
   it, once per machine and keyboard, and follow its readings.
4. Show a lesson with son clave on `"keys": "f"` and `"mute": false`,
   select the block and press `space`: each tap sounds a clave with no audible flam against the key's
   own click; dots land near the targets; muting voice 1 silences the
   computer's clave but not the taps.
5. Click the terminal and type `f`: no sound.
6. `djembot stop`, `djembot report`: per-note rows for pulses 2, 4, 8, 11,
   14; the run path is printed and the file exists.
7. Show a two-block score, a listen-only head chained to a part block,
   armed from the head:

   ````sh
   djembot show - --play lead <<'EOF'
   ```djembot
   { "label": "lead", "tempo": 100, "beats": 4, "cycles": 2, "count_in": 4,
     "next": "attacca",
     "voices": [ { "name": "click", "instrument": "click", "pulses": 4,
                   "onsets": [0, 1, 2, 3] } ] }
   ```

   ```djembot
   { "label": "part", "tempo": 100, "beats": 4, "cycles": 4, "count_in": 4,
     "voices": [ { "name": "click", "instrument": "click", "pulses": 4,
                   "onsets": [0, 1, 2, 3], "keys": "f", "mute": false } ] }
   ```
   EOF
   ````

   It prints `armed lead — the user starts it with space (or ▶)`; the
   window is silent and `lead`'s header reads `press space to start`.
   In a second terminal run `djembot wait`: it prints `armed label=lead`
   and waits. Press `space`: `lead` counts in and plays two cycles, then
   `part` begins on the next beat with no count-in, the outline and the
   scroll move to it, and it ends on the downbeat after four cycles.
   `djembot wait` has printed `started label=lead`, `ended label=lead
   by=end next=part cycles=2/2`, `started label=part`, then `ended
   label=part by=end cycles=4/4 hit=… run=…` and exited 0.
8. `cargo test` passes; `cargo test -- --include-ignored` passes.
9. With a looping part block playing and the window focused, hide the
   window — switch to the workspace with the terminal, or cover it — and
   type the bound key in another application: no sound, and the stroke is
   not in the report. With the window still hidden, `djembot report`
   answers with the run so far and `djembot stop` stops the block, both at
   once, not after 5 s. Switch back: the window shows the block stopped and
   the run path in the status line. On a finite block, let it end while
   hidden; `djembot report` then shows `ended    end` and the run path.
10. `djembot init` into an empty directory prints `wrote CLAUDE.md`,
    `wrote JOURNAL.md`, eleven `wrote library/….json` lines and `wrote
    lessons/000-twelve-vs-sixteen.md`, then the `next:` line; a second run
    prints `kept … (same as this version)` for each and changes nothing.
