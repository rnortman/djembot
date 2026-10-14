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
as one run file with the cycle count grown or one per pass. The
standing line it adds is gated by the block's `feedback`, and
`"cycle_count": "countdown"` counts down the current pass and reads
`another 8` when the decision is to repeat.

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

## `quality-needs-cycles-rule-split-across-modules`

Two rules of one kind — a field value that needs `cycles`, because a
looping block never ends — live in two places. `cycle_count:
"countdown"` is checked in `Exercise::from_value`, so it holds for every
exercise, including one deserialised from the socket; `next: "attacca"`
is checked in `lesson::parse`, beside the lesson-level "but no block
follows" rule, so an exercise built outside a lesson can carry it.
Moving the attacca half into `from_value` beside the countdown check
(one helper for both messages) makes `from_value` the whole
single-exercise validator, but changes what an author sees for a last
block with `next: "attacca"` and no `cycles`: the block fails to parse,
so only the `needs cycles` error is reported, not both.
