# TODOs

Deferred work, joined to the code by slug: an entry `## \`slug\`` here
is referenced from the code as `// TODO(slug)`. Completing one removes
both.

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

## `runs-in-learning-dir`

Run files go to `$XDG_DATA_HOME/djembot/runs`, outside the learning
directory, so the evidence behind a session's write-up is not in the
learner's git repository and dies with the machine or the run format.
The instructor copies each `run=` path that `wait` prints into the
session's `log/YYYY/MM/DD-NN/` directory by hand.

Change: let the window write runs where the learning directory wants
them: `djembot gui --runs DIR` (or `DJEMBOT_RUNS`), or let `show` pass
the lesson's learning directory along so runs land in its current
session directory. The run filename already carries timestamp and label.
Then the contract drops the copying step.

## `closing-tap-annotation`

A stroke more than a window after the closing "one" is kept for a beat
and reported as an extra annotated to the nearest target of its own
voice, which the report counts as *wide* only when that target is the
missed closing one. On the last block the voice's unplayed pulses after
the end are also targets an extra may annotate, so a dragged closing tap
is wide only while it is nearer the closing one than the voice's next
onset: less than half that gap late. When half the gap is inside the
window, no dragged closing tap is ever wide: at 120 bpm with the default
100 ms window, a voice whose next onset is one sixteenth (125 ms) or one
triplet pulse (167 ms) after the one has its halfway point at 62 or 83
ms, inside the window; two sixteenths after leaves a 25 ms sliver. The
report then shows a miss and a plain extra, which the instructor
contract reads as losing the beat plus a stray stroke, and the
dragged-last-tap signal exists only on voices with room after the one.

One candidate: when annotating an extra, prefer a missed instance over
one that was never due — at the end of a score, the missed closing one
over an unplayed later pulse. Stated generally it changes what every
annotation in a run means, and with it the report's wide counts and the
pulse and cycle its per-note and over-time rows file each extra under;
the grid and necklace draw an extra where it was struck and are
unaffected. Stated for the closing one only it is a special case of the
annotation rule.

## `correctness-judged-block-never-ends-when-score-ends-inside-its-trail`

When a block with a part is followed attacca by a last block that ends
before the first block's judged span closes (a one-beat closer, or a
short fast tail after a wide window), the first block is never retired
while the score plays. Once the mixer stops, the snapshot sits at the
last block's end, not the first block's, so `judge_hits` has no
reference clock for it and does not count it as finished; meanwhile
`follow_score` emits the last block's terminal `ended`, and `wait`
returns without the first block's `run=`. The performance stays open
until a later stop, restart, switch or quit retires it under that `by=`
instead of `by=end`, and its run file is written only then.

Needs a decision on what the window does at that point: keep judging the
open span against an extrapolated clock and make the terminal event wait
for it, or close every open span when the mixer stops at the score's end
and emit those `ended` lines before the terminal one. Either way the
terminal `ended` must be the last event of a played-through score, and a
test should cover a one-beat closing block.

## `correctness-negative-offset-ends-judged-block-early-and-terminal`

`Performance::closed` is `heard - offset > end + trail`, and the
calibration offset is not range-restricted. With an offset more negative
than minus the trail (a 300 bpm successor with the default window gives
a 200 ms trail; a measured offset can approach the calibration block's
300 ms window, and an edited calibration file can hold anything), a
judged block closes before the heard clock reaches the next block. Its
`ended` then precedes `started` of the next block, and if the mixer has
not yet swapped segments `successor` yields no `next`, so the attacca
`ended` is terminal and `wait` returns while the score is still playing.

Candidates: do not let a span close before the raw heard clock has
passed the block's end; bound the offset in `Calibration::load` and in
calibration itself; or both. Which one is right depends on what a strongly
negative offset should mean for judging, not only for the event stream.
