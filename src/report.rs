//! The plain-text report of a run: what the instructor reads after (or
//! during) a performance. A pure function of the run, so a saved run file
//! can be reported again at any time.

use std::collections::{BTreeMap, HashSet};
use std::fmt::Write as _;
use std::path::Path;

use crate::model::Ending;
use crate::perf::{self, EndedBy, Run, RunHit, RunJudgment, Timing};

/// Rows the over-time section groups the cycles into, at most.
const TIME_BINS: usize = 8;

/// Renders `run`; `path` is where its file is, `None` when it has not been
/// written.
pub fn render(run: &Run, path: Option<&Path>) -> String {
    let mut out = String::new();
    header(&mut out, run, path);
    let missed = Missed::of(run);
    let all = Group::of(run, &missed, |_| true);
    out.push_str("\nOverall\n");
    out.push_str(&table(&GROUP_HEAD, vec![all.row("all")]));
    out.push_str("\nPer part\n");
    let parts = run
        .parts
        .iter()
        .map(|p| Group::of(run, &missed, |v| v == p.voice).row(&format!("{} {}", p.voice, p.name)))
        .collect();
    out.push_str(&table(&GROUP_HEAD, parts));
    out.push_str("\nPer note\n");
    out.push_str(&per_note(run, &missed));
    out.push_str("\nOver time\n");
    out.push_str(&over_time(run, &missed));
    out
}

/// The run's missed instances, `(voice, cycle, pulse)`.
struct Missed(HashSet<(usize, i64, u32)>);

impl Missed {
    fn of(run: &Run) -> Missed {
        Missed(
            run.misses
                .iter()
                .map(|m| (m.voice, m.cycle as i64, m.pulse))
                .collect(),
        )
    }

    /// Whether `h` is an extra whose nearest instance was missed: the
    /// user's attempt at that target, outside the window.
    fn wide(&self, h: &RunHit) -> bool {
        h.judgment == RunJudgment::Extra
            && h.pulse
                .is_some_and(|p| self.0.contains(&(h.voice, h.cycle, p)))
    }
}

fn header(out: &mut String, run: &Run, path: Option<&Path>) {
    let block = match &run.title {
        Some(t) => format!("{} — {t}", run.label),
        None => run.label.clone(),
    };
    let cycles = match (run.cycles, run.ending) {
        (Some(c), Ending::Downbeat) => format!(
            "{} of {c} cycles played, ending on the downbeat",
            run.cycles_played
        ),
        (Some(c), Ending::None) => format!(
            "{} of {c} cycles played, no closing stroke",
            run.cycles_played
        ),
        (None, _) => format!("{} cycles played (looping)", run.cycles_played),
    };
    let file = match path {
        Some(p) => p.display().to_string(),
        None if run.ended_by == EndedBy::Open => "not written yet (still playing)".into(),
        None => "not written".into(),
    };
    let ended_by = run.ended_by.as_str();
    let rows = [
        ("lesson", run.lesson.clone()),
        ("block", block),
        (
            "tempo",
            format!(
                "{} bpm · {} beats · count-in {}",
                num(run.tempo),
                run.beats,
                run.count_in
            ),
        ),
        ("cycles", cycles),
        ("window", format!("±{} ms", num(run.window_ms))),
        ("offset", format!("{:+.1} ms (calibration)", run.offset_ms)),
        (
            "latency",
            match run.latency_ms {
                Some(l) => format!("{l:.1} ms (output, device-reported)"),
                None => "–".into(),
            },
        ),
        ("input", run.input.clone()),
        ("run", file),
        (
            "ended",
            format!("{ended_by} ({} → {})", run.started, run.ended),
        ),
    ];
    for (k, v) in rows {
        let _ = writeln!(out, "{k:<8} {v}");
    }
}

/// A number without a trailing `.0`.
fn num(x: f64) -> String {
    if x.fract() == 0.0 {
        format!("{x:.0}")
    } else {
        format!("{x}")
    }
}

const GROUP_HEAD: [&str; 21] = [
    "",
    "targets",
    "hit",
    "miss",
    "extra",
    "wide",
    "wrong",
    "early",
    "on",
    "late",
    "mean",
    "median",
    "sd",
    "min",
    "max",
    "p10",
    "p25",
    "p75",
    "p90",
    "|mean|",
    "wide mean",
];

/// The counts and the signed errors of the strokes and targets on some
/// voices.
struct Group {
    hit: usize,
    miss: usize,
    extra: usize,
    wrong_tone: usize,
    errors: Vec<f64>,
    /// The signed errors of the wide extras.
    wide: Vec<f64>,
}

impl Group {
    /// The group of the voices `keep` accepts (1-based voice numbers).
    fn of(run: &Run, missed: &Missed, keep: impl Fn(usize) -> bool) -> Group {
        let mut g = Group {
            hit: 0,
            miss: run.misses.iter().filter(|m| keep(m.voice)).count(),
            extra: 0,
            wrong_tone: 0,
            errors: Vec::new(),
            wide: Vec::new(),
        };
        for h in run.hits.iter().filter(|h| keep(h.voice)) {
            match h.judgment {
                RunJudgment::Hit => {
                    g.hit += 1;
                    if h.target_tone.is_some_and(|t| t != h.tone) {
                        g.wrong_tone += 1;
                    }
                    g.errors.extend(h.error_ms);
                }
                RunJudgment::Extra => {
                    g.extra += 1;
                    if missed.wide(h) {
                        g.wide.extend(h.error_ms);
                    }
                }
                RunJudgment::Ignored => {}
            }
        }
        g
    }

    fn row(&self, name: &str) -> Vec<String> {
        let (early, on, late) = timings(&self.errors);
        let s = Stats::of(&self.errors);
        let mut row = vec![
            name.to_string(),
            (self.hit + self.miss).to_string(),
            self.hit.to_string(),
            self.miss.to_string(),
            self.extra.to_string(),
            self.wide.len().to_string(),
            self.wrong_tone.to_string(),
            early.to_string(),
            on.to_string(),
            late.to_string(),
        ];
        row.extend(
            [
                s.mean, s.median, s.sd, s.min, s.max, s.p10, s.p25, s.p75, s.p90,
            ]
            .into_iter()
            .zip([true, true, false, true, true, true, true, true, true])
            .map(|(x, signed)| ms(x, signed)),
        );
        row.push(ms(s.mean_abs, false));
        row.push(ms(Stats::of(&self.wide).mean, true));
        row
    }
}

/// Early, on and late counts of `errors`.
fn timings(errors: &[f64]) -> (usize, usize, usize) {
    let count = |t: Timing| errors.iter().filter(|&&e| perf::timing(e) == t).count();
    (count(Timing::Early), count(Timing::On), count(Timing::Late))
}

/// Summary statistics of signed errors in ms; every field `None` when
/// there are none (`sd` also with a single value).
#[derive(Debug, Default, PartialEq)]
struct Stats {
    mean: Option<f64>,
    median: Option<f64>,
    sd: Option<f64>,
    min: Option<f64>,
    max: Option<f64>,
    p10: Option<f64>,
    p25: Option<f64>,
    p75: Option<f64>,
    p90: Option<f64>,
    mean_abs: Option<f64>,
}

impl Stats {
    fn of(xs: &[f64]) -> Stats {
        if xs.is_empty() {
            return Stats::default();
        }
        let n = xs.len() as f64;
        let mut sorted = xs.to_vec();
        sorted.sort_by(f64::total_cmp);
        Stats {
            mean: Some(xs.iter().sum::<f64>() / n),
            median: perf::median(xs),
            sd: perf::sample_sd(xs),
            min: sorted.first().copied(),
            max: sorted.last().copied(),
            p10: percentile(&sorted, 10.0),
            p25: percentile(&sorted, 25.0),
            p75: percentile(&sorted, 75.0),
            p90: percentile(&sorted, 90.0),
            mean_abs: Some(xs.iter().map(|x| x.abs()).sum::<f64>() / n),
        }
    }
}

/// The nearest-rank `p`th percentile of `sorted` (ascending): the value of
/// rank `⌈p/100 · n⌉`, at least the first.
fn percentile(sorted: &[f64], p: f64) -> Option<f64> {
    if sorted.is_empty() {
        return None;
    }
    let rank = (p / 100.0 * sorted.len() as f64).ceil() as usize;
    Some(sorted[rank.clamp(1, sorted.len()) - 1])
}

/// Milliseconds with one decimal, `+`-signed when `signed`; `–` for none.
fn ms(x: Option<f64>, signed: bool) -> String {
    match x {
        // `-0.0` prints as `+0.0`.
        Some(x) if signed => format!("{:+.1}", x + 0.0),
        Some(x) => format!("{x:.1}"),
        None => "–".into(),
    }
}

/// One row per `(voice, pulse)` target.
fn per_note(run: &Run, missed: &Missed) -> String {
    #[derive(Default)]
    struct Note {
        hit: usize,
        miss: usize,
        wide: usize,
        errors: Vec<f64>,
    }
    let mut notes: BTreeMap<(usize, u32), Note> = BTreeMap::new();
    for p in &run.parts {
        for &pulse in &p.onsets {
            notes.entry((p.voice, pulse)).or_default();
        }
    }
    for h in &run.hits {
        if let (RunJudgment::Hit, Some(pulse)) = (h.judgment, h.pulse) {
            let n = notes.entry((h.voice, pulse)).or_default();
            n.hit += 1;
            n.errors.extend(h.error_ms);
        }
        if let (true, Some(pulse)) = (missed.wide(h), h.pulse) {
            notes.entry((h.voice, pulse)).or_default().wide += 1;
        }
    }
    for m in &run.misses {
        notes.entry((m.voice, m.pulse)).or_default().miss += 1;
    }
    let rows = notes
        .iter()
        .map(|(&(voice, pulse), n)| {
            let (early, on, late) = timings(&n.errors);
            let s = Stats::of(&n.errors);
            vec![
                voice.to_string(),
                pulse.to_string(),
                (n.hit + n.miss).to_string(),
                n.hit.to_string(),
                n.miss.to_string(),
                n.wide.to_string(),
                early.to_string(),
                on.to_string(),
                late.to_string(),
                ms(s.mean, true),
                ms(s.median, true),
                ms(s.sd, false),
            ]
        })
        .collect();
    table(
        &[
            "voice", "pulse", "n", "hit", "miss", "wide", "early", "on", "late", "mean", "median",
            "sd",
        ],
        rows,
    )
}

/// The cycles in at most `TIME_BINS` equal groups (one per cycle when there
/// are no more), numbered from 1.
fn over_time(run: &Run, missed: &Missed) -> String {
    // A judgment past the cycles begun (an early hit on the next "one", the
    // closing instance, an extra nearest the next cycle) folds into the last.
    let judged =
        run.hits.iter().any(|h| h.judgment != RunJudgment::Ignored) || !run.misses.is_empty();
    let n = (run.cycles_played as usize).max(usize::from(judged));
    if n == 0 {
        return table(&OVER_TIME_HEAD, vec![]);
    }
    let bins = n.min(TIME_BINS);
    let bin_of = |cycle: i64| -> usize {
        let c = cycle.clamp(0, n as i64 - 1) as usize;
        // Bin `b` holds cycles `b·n/bins .. (b+1)·n/bins`.
        (0..bins)
            .rev()
            .find(|&b| b * n / bins <= c)
            .expect("bin 0 starts at cycle 0")
    };
    #[derive(Clone, Default)]
    struct Bin {
        hit: usize,
        miss: usize,
        extra: usize,
        wide: usize,
        errors: Vec<f64>,
    }
    let mut groups: Vec<Bin> = vec![Bin::default(); bins];
    for h in &run.hits {
        let g = &mut groups[bin_of(h.cycle)];
        match h.judgment {
            RunJudgment::Hit => {
                g.hit += 1;
                g.errors.extend(h.error_ms);
            }
            RunJudgment::Extra => {
                g.extra += 1;
                g.wide += usize::from(missed.wide(h));
            }
            RunJudgment::Ignored => {}
        }
    }
    for m in &run.misses {
        groups[bin_of(m.cycle as i64)].miss += 1;
    }
    let rows = groups
        .iter()
        .enumerate()
        .map(|(b, g)| {
            let (first, last) = (b * n / bins + 1, (b + 1) * n / bins);
            let s = Stats::of(&g.errors);
            vec![
                if first == last {
                    first.to_string()
                } else {
                    format!("{first}–{last}")
                },
                g.hit.to_string(),
                g.miss.to_string(),
                g.extra.to_string(),
                g.wide.to_string(),
                ms(s.mean, true),
                ms(s.sd, false),
            ]
        })
        .collect();
    table(&OVER_TIME_HEAD, rows)
}

const OVER_TIME_HEAD: [&str; 7] = ["cycles", "hit", "miss", "extra", "wide", "mean", "sd"];

/// Aligned columns, indented two spaces: the first left-aligned, the rest
/// right-aligned. A table without rows says so.
fn table(head: &[&str], rows: Vec<Vec<String>>) -> String {
    if rows.is_empty() {
        return "  (none)\n".into();
    }
    let width = |i: usize| {
        rows.iter()
            .map(|r| r[i].chars().count())
            .chain([head[i].chars().count()])
            .max()
            .unwrap_or(0)
    };
    let widths: Vec<usize> = (0..head.len()).map(width).collect();
    let line = |cells: Vec<&str>| {
        let mut s = String::from("  ");
        for (i, c) in cells.iter().enumerate() {
            let pad = widths[i] - c.chars().count();
            if i == 0 {
                s.push_str(c);
                s.push_str(&" ".repeat(pad));
            } else {
                s.push_str(&" ".repeat(pad + 2));
                s.push_str(c);
            }
        }
        s.truncate(s.trim_end().len());
        s.push('\n');
        s
    };
    let mut out = line(head.to_vec());
    for r in &rows {
        out.push_str(&line(r.iter().map(String::as_str).collect()));
    }
    out
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use crate::perf::{RUN_FORMAT, RunHit, RunMiss, RunPart};
    use std::path::PathBuf;

    /// The window's live counts and the report's overall row are computed
    /// separately, from the performance in frames and from its run in ms;
    /// they agree on the same strokes.
    #[test]
    fn the_live_tally_agrees_with_the_overall_group() {
        use crate::perf::Performance;
        use crate::sounds::Bank;
        use crate::timeline::Timeline;
        use crate::timeline::tests::exercise;
        use std::sync::Arc;

        const RATE: u32 = 48_000;
        let ex = exercise(serde_json::json!({
            "tempo": 100, "beats": 4, "cycles": 8, "count_in": 4, "window_ms": 150,
            "voices": [{ "name": "click", "instrument": "click", "pulses": 4,
                         "onsets": [0, 1, 2, 3], "keys": "f" }]
        }));
        let mut bank = Bank::from_fn(|_| Arc::from(vec![1.0f32]));
        let tl = Timeline::from(&ex, &mut bank, RATE).unwrap();
        let mut p = Performance::new(
            &ex,
            &tl,
            0,
            Default::default(),
            RATE,
            perf::conditions("l", 0.0, Some(9.0)),
        );
        // 100 bpm: 28 800 frames a beat; cycle 0 starts after 4 beats.
        let beat = 28_800.0;
        let start = 4.0 * beat;
        p.judge(1_000.0, 0, 'x', None); // ignored: before the count-in's last window
        p.judge(start + 480.0, 0, 'x', None); // +10 ms
        p.judge(start + beat - 960.0, 0, 'x', None); // -20 ms
        p.judge(start + beat + 100.0, 0, 'x', None); // extra: nearest claimed
        p.judge(start + 2.0 * beat + 240.0, 0, 'x', None); // +5 ms
        p.advance(start + 4.0 * beat + 7_201.0); // beat 3 and cycle 1 beat 0 missed

        let t = p.tally();
        let run = p.run(EndedBy::Open);
        let g = Group::of(&run, &Missed::of(&run), |_| true);
        assert_eq!((t.hit, t.miss, t.extra), (g.hit, g.miss, g.extra));
        assert_eq!((t.hit, t.miss, t.extra), (3, 2, 1));
        let mean = Stats::of(&g.errors).mean.unwrap();
        assert!((t.mean_ms.unwrap() - mean).abs() < 0.05, "{t:?} vs {mean}");
        assert!((mean + 5.0 / 3.0).abs() < 0.05, "{mean}");
    }

    fn part(voice: usize, name: &str, onsets: &[u32]) -> RunPart {
        RunPart {
            voice,
            name: name.into(),
            instrument: "clave".into(),
            keys: [("x".to_string(), "f".to_string())].into(),
            pulses: 16,
            onsets: onsets.to_vec(),
        }
    }

    fn hit(voice: usize, cycle: i64, pulse: u32, error_ms: f64) -> RunHit {
        RunHit {
            t_ms: 0.0,
            voice,
            tone: 'x',
            cycle,
            pulse: Some(pulse),
            target_tone: Some('x'),
            error_ms: Some(error_ms),
            judgment: RunJudgment::Hit,
            velocity: None,
        }
    }

    fn unmatched(voice: usize, cycle: i64, judgment: RunJudgment) -> RunHit {
        RunHit {
            pulse: None,
            target_tone: None,
            error_ms: None,
            judgment,
            ..hit(voice, cycle, 0, 0.0)
        }
    }

    fn miss(voice: usize, cycle: u32, pulse: u32) -> RunMiss {
        RunMiss {
            cycle,
            voice,
            pulse,
        }
    }

    fn run(hits: Vec<RunHit>, misses: Vec<RunMiss>) -> Run {
        Run {
            djembot: RUN_FORMAT,
            lesson: "/abs/lessons/004-clave-on-f.md".into(),
            label: "A".into(),
            title: Some("Son clave 2-3".into()),
            tempo: 90.0,
            beats: 4,
            cycles: Some(2),
            ending: Ending::None,
            count_in: 4,
            window_ms: 100.0,
            offset_ms: 12.0,
            latency_ms: Some(9.04),
            input: "2 devices".into(),
            started: "2026-10-11T10:12:03Z".into(),
            ended: "2026-10-11T10:12:31Z".into(),
            ended_by: EndedBy::End,
            cycles_played: 2,
            parts: vec![part(1, "clave", &[2, 4])],
            hits,
            misses,
        }
    }

    /// The data row of the single-row table in `section`.
    /// The cells of the first row of `section` whose first cell is `first`.
    pub fn row_of<'a>(text: &'a str, section: &str, first: &str) -> Vec<&'a str> {
        let at = text.find(&format!("\n{section}\n")).expect(section);
        text[at..]
            .lines()
            .map(str::split_whitespace)
            .map(Iterator::collect::<Vec<_>>)
            .find(|cells| cells.first() == Some(&first))
            .unwrap_or_else(|| panic!("no row {first} in {section}:\n{text}"))
    }

    #[test]
    fn statistics_by_hand() {
        // Sorted: -30 -10 0 10 25 35; n = 6.
        let s = Stats::of(&[10.0, -10.0, 35.0, 0.0, -30.0, 25.0]);
        assert_eq!(s.mean, Some(5.0));
        assert_eq!(s.median, Some(5.0));
        // Squared deviations 25 225 900 25 1225 400 = 2800; / 5 = 560.
        assert!((s.sd.unwrap() - 560f64.sqrt()).abs() < 1e-9);
        assert_eq!((s.min, s.max), (Some(-30.0), Some(35.0)));
        // Ranks ⌈0.6⌉=1, ⌈1.5⌉=2, ⌈4.5⌉=5, ⌈5.4⌉=6.
        assert_eq!(
            (s.p10, s.p25, s.p75, s.p90),
            (Some(-30.0), Some(-10.0), Some(25.0), Some(35.0))
        );
        assert!((s.mean_abs.unwrap() - 110.0 / 6.0).abs() < 1e-9);

        let one = Stats::of(&[-4.0]);
        assert_eq!((one.mean, one.sd, one.p90), (Some(-4.0), None, Some(-4.0)));
        assert_eq!(Stats::of(&[]), Stats::default());
    }

    #[test]
    fn overall_counts_and_errors() {
        let mut wrong = hit(1, 1, 4, 30.0);
        wrong.tone = 'y';
        let r = run(
            vec![
                hit(1, 0, 2, -25.0),
                hit(1, 0, 4, 5.0),
                hit(1, 1, 2, 20.0),
                wrong,
                unmatched(1, 1, RunJudgment::Extra),
                unmatched(1, -1, RunJudgment::Ignored),
            ],
            vec![],
        );
        let text = render(&r, None);
        assert_eq!(
            row_of(&text, "Overall", "all"),
            vec![
                "all", "4", "4", "0", "1", "0", "1", // targets hit miss extra wide wrong
                "1", "2", "1", // early on late (±20 is on)
                "+7.5", "+12.5", "24.0", "-25.0", "+30.0", // mean median sd min max
                "-25.0", "-25.0", "+20.0", "+30.0", "20.0", // p10 p25 p75 p90 |mean|
                "–",    // wide mean
            ]
        );
    }

    #[test]
    fn empty_groups_print_a_dash() {
        let r = run(vec![], vec![miss(1, 0, 2)]);
        let text = render(&r, None);
        let all = row_of(&text, "Overall", "all");
        assert_eq!(&all[1..6], ["1", "0", "1", "0", "0"]);
        assert!(all[10..].iter().all(|c| *c == "–"), "{all:?}");
        // A pulse never struck has a row of its own with dashes.
        let note = row_of(&text, "Per note", "1");
        assert_eq!(
            note,
            vec!["1", "2", "1", "0", "1", "0", "0", "0", "0", "–", "–", "–"]
        );
    }

    #[test]
    fn per_part_and_per_note_rows() {
        let mut r = run(
            vec![
                hit(1, 0, 2, -10.0),
                hit(1, 1, 2, -30.0),
                hit(1, 0, 4, 40.0),
                hit(2, 0, 0, 0.0),
            ],
            vec![miss(1, 1, 4)],
        );
        r.parts.push(part(2, "bell", &[0]));
        let text = render(&r, None);
        let clave = row_of(&text, "Per part", "1");
        assert_eq!(clave[1], "clave");
        assert_eq!(&clave[2..6], ["4", "3", "1", "0"]);
        let bell = row_of(&text, "Per part", "2");
        assert_eq!(&bell[2..6], ["1", "1", "0", "0"]);

        let at = text.find("\nPer note\n").unwrap();
        let end = text.find("\nOver time\n").unwrap();
        let rows: Vec<Vec<&str>> = text[at..end]
            .lines()
            .skip(3)
            .map(|l| l.split_whitespace().collect())
            .collect();
        assert_eq!(
            rows,
            vec![
                vec![
                    "1", "2", "2", "2", "0", "0", "1", "1", "0", "-20.0", "-20.0", "14.1"
                ],
                vec![
                    "1", "4", "2", "1", "1", "0", "0", "0", "1", "+40.0", "+40.0", "–"
                ],
                vec![
                    "2", "0", "1", "1", "0", "0", "0", "1", "0", "+0.0", "+0.0", "–"
                ],
            ]
        );
    }

    #[test]
    fn twenty_cycles_bin_into_eight_rows() {
        let mut r = run(
            (0..20).map(|c| hit(1, c, 2, c as f64)).collect(),
            vec![miss(1, 19, 4)],
        );
        r.cycles = None;
        r.cycles_played = 20;
        r.ended_by = EndedBy::Stop;
        r.hits.push(unmatched(1, 3, RunJudgment::Extra));
        let text = render(&r, None);
        let at = text.find("\nOver time\n").unwrap();
        let rows: Vec<Vec<&str>> = text[at..]
            .lines()
            .skip(3)
            .map(|l| l.split_whitespace().collect())
            .collect();
        let labels: Vec<&str> = rows.iter().map(|r| r[0]).collect();
        assert_eq!(
            labels,
            [
                "1–2", "3–5", "6–7", "8–10", "11–12", "13–15", "16–17", "18–20"
            ]
        );
        // Cycles 3–5 are 0-based 2, 3, 4: errors 2, 3, 4 and the extra.
        assert_eq!(rows[1], ["3–5", "3", "0", "1", "0", "+3.0", "1.0"]);
        assert_eq!(rows[7], ["18–20", "3", "1", "0", "0", "+18.0", "1.0"]);
    }

    #[test]
    fn a_short_run_has_a_row_per_cycle() {
        let r = run(vec![hit(1, 0, 2, 1.0), hit(1, 1, 2, 3.0)], vec![]);
        let text = render(&r, None);
        assert_eq!(
            row_of(&text, "Over time", "1"),
            ["1", "1", "0", "0", "0", "+1.0", "–"]
        );
        assert_eq!(
            row_of(&text, "Over time", "2"),
            ["2", "1", "0", "0", "0", "+3.0", "–"]
        );

        let mut none = run(vec![], vec![]);
        none.cycles_played = 0;
        assert!(render(&none, None).ends_with("Over time\n  (none)\n"));
    }

    #[test]
    fn an_extra_nearest_the_next_cycle_of_a_looping_run_folds_into_the_last_played() {
        let mut r = run(
            vec![hit(1, 0, 2, 1.0), hit(1, 1, 2, 3.0)],
            vec![miss(1, 1, 4)],
        );
        r.cycles = None;
        r.ended_by = EndedBy::Stop;
        // Late in cycle index 1, nearest cycle index 2's pulse 2.
        r.hits.push(RunHit {
            pulse: Some(2),
            error_ms: Some(-150.0),
            target_tone: None,
            ..unmatched(1, 2, RunJudgment::Extra)
        });
        let text = render(&r, None);
        let at = text.find("\nOver time\n").unwrap();
        let rows: Vec<Vec<&str>> = text[at..]
            .lines()
            .skip(3)
            .map(|l| l.split_whitespace().collect())
            .collect();
        assert_eq!(
            rows,
            [
                vec!["1", "1", "0", "0", "0", "+1.0", "–"],
                vec!["2", "1", "1", "1", "0", "+3.0", "–"],
            ],
            "{text}"
        );
    }

    #[test]
    fn header_shows_the_conditions() {
        let r = run(vec![], vec![]);
        let path = PathBuf::from("/home/u/.local/share/djembot/runs/20261011-101203-A.json");
        let text = render(&r, Some(&path));
        let head: Vec<&str> = text.lines().take(10).collect();
        assert_eq!(
            head,
            [
                "lesson   /abs/lessons/004-clave-on-f.md",
                "block    A — Son clave 2-3",
                "tempo    90 bpm · 4 beats · count-in 4",
                "cycles   2 of 2 cycles played, no closing stroke",
                "window   ±100 ms",
                "offset   +12.0 ms (calibration)",
                "latency  9.0 ms (output, device-reported)",
                "input    2 devices",
                "run      /home/u/.local/share/djembot/runs/20261011-101203-A.json",
                "ended    end (2026-10-11T10:12:03Z → 2026-10-11T10:12:31Z)",
            ]
        );

        let mut open = run(vec![], vec![]);
        open.ended_by = EndedBy::Open;
        open.cycles = None;
        open.input = "no access to /dev/input/event3".into();
        open.latency_ms = None;
        let text = render(&open, None);
        assert!(
            text.contains("\ncycles   2 cycles played (looping)\n"),
            "{text}"
        );
        assert!(text.contains("\ninput    no access to /dev/input/event3\n"));
        assert!(text.contains("\nrun      not written yet (still playing)\n"));
        assert!(text.contains("\nended    open ("));
        assert!(text.contains("\nlatency  –\n"), "{text}");
    }

    /// A recorded run (60 bpm, 4 beats, 8 cycles, window
    /// 100 ms, a part on every beat), its strokes replayed as recorded
    /// (`t_ms`, already calibrated) without a closing downbeat, as it was
    /// played.
    fn bell_slow() -> Run {
        use crate::perf::Performance;
        use crate::sounds::Bank;
        use crate::timeline::Timeline;
        use crate::timeline::tests::exercise;
        use std::sync::Arc;

        const RATE: u32 = 48_000;
        const T_MS: [f64; 33] = [
            -114.00, 829.71, 1877.62, 3013.76, 3989.66, 4965.59, 5941.29, 6965.43, 7989.35,
            8885.07, 9916.99, 10876.99, 11884.80, 13100.71, 13892.68, 14916.59, 15988.50, 16948.39,
            17788.59, 18932.49, 19996.18, 20964.41, 21948.00, 23011.94, 24036.28, 25011.98,
            25971.74, 27011.79, 27971.52, 28971.64, 29931.56, 30987.49, 31987.20,
        ];
        let ex = exercise(serde_json::json!({
            "label": "bell-slow", "tempo": 60, "beats": 4, "cycles": 8, "count_in": 4,
            "window_ms": 100, "ending": "none",
            "voices": [{ "name": "feet", "instrument": "click", "pulses": 4,
                         "onsets": [0, 1, 2, 3], "keys": "f" }]
        }));
        let mut bank = Bank::from_fn(|_| Arc::from(vec![1.0f32]));
        let tl = Timeline::from(&ex, &mut bank, RATE).unwrap();
        let mut p = Performance::new(
            &ex,
            &tl,
            0,
            Default::default(),
            RATE,
            perf::conditions("l", 0.0, None),
        );
        let start = tl.count_in_len as f64;
        for t in T_MS {
            p.judge(start + t * 48.0, 0, 'x', None);
        }
        p.finish();
        p.run(EndedBy::End)
    }

    #[test]
    fn wide_counts_extras_whose_nearest_target_was_missed() {
        let text = render(&bell_slow(), None);
        let all = row_of(&text, "Overall", "all");
        // targets hit miss extra wide; the wide mean is the last column.
        assert_eq!(&all[1..6], ["32", "23", "9", "9", "8"], "{text}");
        assert_eq!(all.last(), Some(&"-108.0"), "{text}");
        let rows = |section: &str, firsts: &[&str]| -> Vec<Vec<String>> {
            firsts
                .iter()
                .map(|f| {
                    row_of(&text, section, f)
                        .iter()
                        .map(|c| c.to_string())
                        .collect()
                })
                .collect()
        };
        // cycles hit miss extra wide.
        let over = rows("Over time", &["1", "3", "4", "5", "8"]);
        let counts: Vec<&[String]> = over.iter().map(|r| &r[..5]).collect();
        assert_eq!(
            counts,
            [
                ["1", "1", "3", "2", "2"],
                ["3", "2", "2", "2", "2"],
                ["4", "1", "3", "3", "3"],
                ["5", "3", "1", "1", "1"],
                ["8", "4", "0", "1", "0"],
            ],
            "{text}"
        );
        // Per note: voice pulse n hit miss wide. Pulse 0 was missed in
        // cycles 0 and 3 and tapped wide only in 3: the stroke before
        // cycle 0 fell in the count-in and was ignored.
        let notes = rows("Per note", &["1"]);
        assert_eq!(&notes[0][..6], ["1", "0", "8", "6", "2", "1"], "{text}");
    }

    #[test]
    fn a_late_closing_tap_is_wide() {
        // The closing "one" of cycle 2 missed, and tapped 150 ms late.
        let late = RunHit {
            judgment: RunJudgment::Extra,
            ..hit(1, 2, 0, 150.0)
        };
        let mut r = run(vec![late], vec![miss(1, 2, 0)]);
        r.ending = Ending::Downbeat;
        r.parts = vec![part(1, "clave", &[0, 15])];
        let text = render(&r, None);
        let all = row_of(&text, "Overall", "all");
        // extra wide; the wide mean is the last column.
        assert_eq!(&all[4..6], ["1", "1"], "{text}");
        assert_eq!(all.last(), Some(&"+150.0"), "{text}");
    }

    #[test]
    fn wide_is_zero_for_format_2_runs() {
        let mut v = serde_json::to_value(bell_slow()).unwrap();
        v["djembot"] = 2.into();
        v.as_object_mut().unwrap().remove("ending");
        for h in v["hits"].as_array_mut().unwrap() {
            if h["judgment"] == "extra" {
                h["pulse"] = serde_json::Value::Null;
                h["error_ms"] = serde_json::Value::Null;
            }
        }
        let old: Run = serde_json::from_value(v).unwrap();
        let text = render(&old, None);
        let all = row_of(&text, "Overall", "all");
        assert_eq!(&all[1..6], ["32", "23", "9", "9", "0"], "{text}");
        assert_eq!(all.last(), Some(&"–"));
        assert!(text.contains("\ncycles   8 of 8 cycles played, no closing stroke\n"));
    }

    #[test]
    fn header_names_the_ending() {
        let mut r = run(vec![], vec![]);
        r.ending = Ending::Downbeat;
        let text = render(&r, None);
        assert!(
            text.contains("\ncycles   2 of 2 cycles played, ending on the downbeat\n"),
            "{text}"
        );
    }

    #[test]
    fn columns_align() {
        let text = table(
            &["", "n", "mean"],
            vec![
                vec!["all".into(), "12".into(), "+1.0".into()],
                vec!["1 clave".into(), "3".into(), "–".into()],
            ],
        );
        assert_eq!(
            text,
            "            n  mean\n  all      12  +1.0\n  1 clave   3     –\n"
        );
    }
}
