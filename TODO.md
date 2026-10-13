# TODOs

Deferred work, joined to the code by slug: an entry `## \`slug\`` here
is referenced from the code as `// TODO(slug)`. Completing one removes
both.

## `closing-tap-trail`

A tap on the closing "one" of a finite block that lands more than one
window late is dropped, not recorded. `Performance::new` sets `trail`
to `reach(window, None)` for the last block of a score, one window, and
`closed()` ignores every stroke after `end + trail`. Anywhere else in
the block the same stroke would be a wide extra, so the report cannot
tell "did not tap the closing one" from "tapped it late": the closing
target reads as a miss with no extra. Early closing taps survive; late
ones do not. With a negative calibration offset the cut-off is even
earlier on the keyboard.

Change: keep the span open after the last target as long as the judging
rules keep it open anywhere else, so a late closing tap is a wide extra:
`trail = max(window, half the gap to the previous target)`, or the
block's beat. The `ended` event and the run file can keep their
schedule as long as strokes heard inside the trail reach the record, or
`retire` waits out the trail. Test: a closing tap `window + 50 ms` late
is a wide extra.

## `advance-on-threshold`

A score section that repeats until the user passes, then continues
attacca. Today `next` is `stop` or `attacca` and `cycles` is fixed, so
the instructor decides by hand, between runs, whether to move on.

Sketch, keeping the current model: `"next": "attacca"` plus a rule such
as `"until": { "hits": 14, "of": 16 }` (or a hit ratio over the last N
cycles), with a `max_cycles` cap so a section cannot loop forever. The
window re-evaluates at each cycle boundary; when the rule holds it lets
the current `cycles` finish and moves on, otherwise it plays another
`cycles` of the same section. The block header says what the rule is and
how the user stands against it at least a cycle before the boundary.
`wait` reports each repeat (`repeated label=X cycles=8`, or `ended …
by=repeat`), and `report` shows the whole stay in the section, either
as one run file with the cycle count grown or one per pass.

## `wait-ended-before-started`

At an attacca boundary `djembot wait` prints `started label=B` before
`ended label=A …`: the window emits `started` when the music reaches
the next block, and A's `ended` only when its judged span closes, a
trail later. Harmless, but the `ended` before the `started` would read
in order. Either hold B's `started` until A's `ended` is emitted, or
document the order in the instructor contract.

## `feedback-mode`

A way to show the score with no live feedback. `playhead: false` still
draws every stroke's hit/miss/early/late mark on the grid and the
necklace, and the user corrects off those marks instead of the sound;
`hidden: true` hides the marks but also the score the user wants to
read, and the header still counts hits, misses and extras live. So a
`playhead: false` run is not an ear measurement, and there is no setting
that gives one with the score on screen.

Change: a block field `"feedback": "live"` (default: marks and counts as
they happen), `"after"` (shown when the block ends or stops), `"none"`
(never on screen; the report still has everything). `playhead` stays
separate: playhead is "where am I", feedback is "how am I doing", and
both independently on or off covers the cases (band-like: both off;
learning the piece: playhead on; checking the ear: feedback after). The
user's own strokes still sound unless `silent`. Keep `playhead: false`
with live marks as a mode: it is a useful middle rung.

## `cycle-countdown`

A way for the user to learn to count cycles. With a score whose blocks
differ, the user must know when the change is coming. The header's cycle
counter (`3/8`) makes them subtract, and nothing says a change is near;
neither can be hidden on its own.

Change: a per-block setting for how the cycle count is shown, with
levels a lesson steps through: `"cycle_count": "countdown"` ("3 to go …
2 … last cycle", the last cycle in a large face: the crutch), `"count"`
(today's `3/8`), `"none"` (the user counts). With `none`, the report
should say whether the user was ready: with attacca into a block whose
part changes, cycle 1 of the new block is the test. A lesson could also
ask for a tap on a bound key at the last "one" of a block and judge it
as a target, so counting can be practised before the score has a change
in it. Interacts with `advance-on-threshold`: if a section can repeat,
the countdown has to say so well ahead ("another 8").

## `journal-log`

The instructor contract (`starter/instructor.md`, *Journal*) has the
instructor append every session to `JOURNAL.md`, and the template
(`starter/JOURNAL.md`) fixes "newest session at the bottom of
*Sessions*". The journal therefore grows without bound and is read in
full at the start of every session: two sessions already made 230
lines, and a learner who practises twice a day would have thousands
within months, most of it stale detail. Git keeps the history anyway,
so the file does not have to.

Change, in the contract, the template and the changelog:

- `JOURNAL.md` is the student's current state, rewritten at the end of
  each session to what the next instructor needs: the student, where
  they are in the curriculum and what to do next, tool notes, and a
  *Latest session* pointer. Nothing is appended.
- Each session gets `log/YYYY/MM/DD-NN/` (`NN` the session's number
  within the day): `session.md`, the write-up of what the user heard,
  played and what the runs showed, naming lessons by number, and beside
  it the session's run files. Nested by year and month so a month's
  listing stays readable for a daily practitioner.
- At session start the instructor reads the journal, then the entry it
  names, and never lists `log/`: that keeps startup context flat
  however long the history gets. At session end: write `session.md`,
  copy the run files in, rewrite the journal, commit.
- `log/` joins `lessons/`, `JOURNAL.md` and `library/` in the contract's
  list of what is the user's. `init` lays down nothing new (an empty
  directory cannot be embedded); the first session creates it.
- An existing learning directory keeps its own copies (`init` reports
  them as differing); the changelog says to update by hand.

Until `runs-in-learning-dir` lands, the copying of run files is the
instructor's job, from the `run=` paths `wait` prints.

## `runs-in-learning-dir`

Run files go to `$XDG_DATA_HOME/djembot/runs`, outside the learning
directory, so the evidence behind a session's write-up is not in the
learner's git repository and dies with the machine or the run format.
With `journal-log`, the instructor copies each `run=` path that `wait`
prints into the session's `log/YYYY/MM/DD-NN/` directory by hand.

Change: let the window write runs where the learning directory wants
them: `djembot gui --runs DIR` (or `DJEMBOT_RUNS`), or let `show` pass
the lesson's learning directory along so runs land in its current
session directory. The run filename already carries timestamp and label.
Then the contract drops the copying step.
