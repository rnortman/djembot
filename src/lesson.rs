//! Lessons: markdown with embedded rhythm blocks.
//!
//! A line that is exactly ```` ```djembot ```` opens a rhythm block and the
//! next line that is exactly ```` ``` ```` closes it; everything else is
//! text. A block body is an exercise object, or an object with `"file"`
//! naming an exercise JSON file (relative paths resolve against the lesson
//! file's directory, or the current directory for a lesson read from
//! stdin) whose top-level fields the other keys override.

use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::model::{self, Diagnostics, Exercise, Next};

pub const FENCE_OPEN: &str = "```djembot";
pub const FENCE_CLOSE: &str = "```";

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Segment {
    /// Raw markdown between rhythm blocks.
    Text(String),
    Rhythm(Exercise),
}

#[derive(Clone, Debug, PartialEq, Default, Serialize, Deserialize)]
pub struct Lesson {
    pub segments: Vec<Segment>,
}

impl Lesson {
    pub fn blocks(&self) -> impl Iterator<Item = &Exercise> {
        self.segments.iter().filter_map(|s| match s {
            Segment::Rhythm(e) => Some(e),
            Segment::Text(_) => None,
        })
    }

    pub fn labels(&self) -> Vec<String> {
        self.blocks().map(|e| e.label.clone()).collect()
    }

    /// Index (0-based, among rhythm blocks) of the block named by `key`: an
    /// exact label match, else a bare 1-based block number.
    pub fn find(&self, key: &str) -> Option<usize> {
        let labels = self.labels();
        labels.iter().position(|l| l == key).or_else(|| {
            key.parse::<usize>()
                .ok()
                .filter(|n| (1..=labels.len()).contains(n))
                .map(|n| n - 1)
        })
    }
}

/// One fenced rhythm block before its body is parsed.
struct RawBlock {
    body: String,
    line: usize,
}

enum RawSegment {
    Text(String),
    Block(RawBlock),
}

fn split(text: &str, diag: &mut Diagnostics) -> Vec<RawSegment> {
    let mut out = Vec::new();
    let mut text_lines: Vec<&str> = Vec::new();
    let mut block: Option<(usize, Vec<&str>)> = None;
    let mut blocks = 0;
    for (i, raw) in text.lines().enumerate() {
        let line = raw.strip_suffix('\r').unwrap_or(raw);
        match &mut block {
            None if line == FENCE_OPEN => {
                flush_text(&mut text_lines, &mut out);
                blocks += 1;
                block = Some((i + 1, Vec::new()));
            }
            None => text_lines.push(line),
            Some((start, body)) => {
                if line == FENCE_CLOSE {
                    out.push(RawSegment::Block(RawBlock {
                        body: body.join("\n"),
                        line: *start,
                    }));
                    block = None;
                } else {
                    body.push(line);
                }
            }
        }
    }
    if let Some((start, _)) = block {
        diag.errors.push(format!(
            "block {blocks}: unclosed ```djembot fence opened at line {start}"
        ));
    }
    flush_text(&mut text_lines, &mut out);
    out
}

fn flush_text(lines: &mut Vec<&str>, out: &mut Vec<RawSegment>) {
    let joined = lines.join("\n");
    lines.clear();
    if !joined.trim().is_empty() {
        out.push(RawSegment::Text(joined));
    }
}

/// Parses and validates lesson markdown. `dir` is the directory relative
/// `file` references resolve against.
pub fn parse(text: &str, dir: &Path, diag: &mut Diagnostics) -> Option<Lesson> {
    let errors_before = diag.errors.len();
    let mut segments = Vec::new();
    // (1-based block index, label) of every block that parsed.
    let mut labels: Vec<(usize, String)> = Vec::new();
    let mut index = 0;
    // The last block that parsed with `"next": "attacca"`.
    let mut attacca: Option<(usize, String)> = None;
    for raw in split(text, diag) {
        match raw {
            RawSegment::Text(t) => segments.push(Segment::Text(t)),
            RawSegment::Block(b) => {
                index += 1;
                if let Some(e) = parse_block(&b, index, dir, diag) {
                    if let Some((first, _)) = labels.iter().find(|(_, l)| *l == e.label) {
                        diag.errors.push(format!(
                            "block {index} \"{}\": duplicate label, also used by block {first}",
                            e.label
                        ));
                    }
                    // `find` reads a bare integer as a block number, so a
                    // numeric label naming another block would shadow it.
                    if let Ok(n) = e.label.parse::<usize>()
                        && n != index
                    {
                        diag.errors.push(format!(
                            "block {index} \"{}\": a numeric label must be the block's own \
                             number ({index}); `djembot play {n}` means block {n}",
                            e.label
                        ));
                    }
                    if e.next == Next::Attacca {
                        // TODO(quality-needs-cycles-rule-split-across-modules)
                        if e.cycles.is_none() {
                            diag.errors.push(format!(
                                "block {index} \"{}\": next: \"attacca\" needs cycles (a \
                                 looping block never ends)",
                                e.label
                            ));
                        }
                        attacca = Some((index, e.label.clone()));
                    }
                    labels.push((index, e.label.clone()));
                    segments.push(Segment::Rhythm(e));
                }
            }
        }
    }
    if let Some((i, label)) = attacca
        && i == index
    {
        diag.errors.push(format!(
            "block {i} \"{label}\": next: \"attacca\" but no block follows"
        ));
    }

    (diag.errors.len() == errors_before).then_some(Lesson { segments })
}

fn parse_block(b: &RawBlock, index: usize, dir: &Path, diag: &mut Diagnostics) -> Option<Exercise> {
    let value: Value = match serde_json::from_str(&b.body) {
        Ok(v) => v,
        Err(e) => {
            diag.errors.push(format!(
                "block {index} (line {}): invalid JSON: {e}",
                b.line
            ));
            return None;
        }
    };
    let ctx = format!("block {index} \"{}\": ", model::label_of(&value, index));
    let value = resolve_file(value, dir, &ctx, diag)?;
    let ctx = format!("block {index} \"{}\": ", model::label_of(&value, index));
    Exercise::from_value(&value, index, &ctx, diag)
}

/// Expands a `{"file": PATH, ...overrides}` block into the merged exercise
/// object; any other value is returned unchanged.
fn resolve_file(value: Value, dir: &Path, ctx: &str, diag: &mut Diagnostics) -> Option<Value> {
    let Value::Object(mut overrides) = value else {
        return Some(value);
    };
    let Some(file) = overrides.remove("file") else {
        return Some(Value::Object(overrides));
    };
    let Some(file) = file.as_str() else {
        diag.errors
            .push(format!("{ctx}file: must be a string path"));
        return None;
    };
    let path = dir.join(file);
    let mut base = read_object(
        &path,
        &format!("{ctx}file \"{file}\" ({}): ", path.display()),
        diag,
    )?;
    base.extend(overrides);
    Some(Value::Object(base))
}

/// Reads the JSON object in the exercise file at `path`. Problems are
/// pushed to `diag` prefixed with `ctx`.
fn read_object(path: &Path, ctx: &str, diag: &mut Diagnostics) -> Option<Map<String, Value>> {
    match std::fs::read_to_string(path) {
        Ok(text) => parse_object(&text, ctx, diag),
        Err(e) => {
            diag.errors.push(format!("{ctx}{e}"));
            None
        }
    }
}

/// Parses `text` as a JSON object. Problems are pushed to `diag` prefixed
/// with `ctx`.
pub fn parse_object(text: &str, ctx: &str, diag: &mut Diagnostics) -> Option<Map<String, Value>> {
    match serde_json::from_str::<Value>(text) {
        Ok(Value::Object(m)) => Some(m),
        Ok(_) => {
            diag.errors.push(format!("{ctx}not a JSON object"));
            None
        }
        Err(e) => {
            diag.errors.push(format!("{ctx}invalid JSON: {e}"));
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    const CLAVE: &str = r#"{ "label": "A", "tempo": 90, "beats": 4, "voices": [
        { "name": "clave", "instrument": "clave", "pulses": 16, "onsets": [0, 3, 6, 10, 12] } ] }"#;

    fn block(body: &str) -> String {
        format!("```djembot\n{body}\n```\n")
    }

    fn parse_str(text: &str, dir: &Path) -> (Option<Lesson>, Diagnostics) {
        let mut d = Diagnostics::default();
        let l = parse(text, dir, &mut d);
        (l, d)
    }

    fn tmpdir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("djembot-test-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn splits_text_rhythm_text() {
        let text = format!(
            "# Title\n\nListen to **A**.\n\n{}\nWhat changed?\n",
            block(CLAVE)
        );
        let (l, d) = parse_str(&text, Path::new("."));
        assert!(d.errors.is_empty(), "{:?}", d.errors);
        let l = l.unwrap();
        assert_eq!(l.segments.len(), 3);
        assert!(matches!(&l.segments[0], Segment::Text(t) if t.contains("Listen to **A**")));
        assert!(matches!(&l.segments[1], Segment::Rhythm(e) if e.label == "A"));
        assert!(matches!(&l.segments[2], Segment::Text(t) if t.trim() == "What changed?"));
    }

    #[test]
    fn other_fences_are_text() {
        let text = "```rust\nfn main() {}\n```\n";
        let l = parse_str(text, Path::new(".")).0.unwrap();
        assert_eq!(l.segments, vec![Segment::Text(text.trim_end().to_string())]);
    }

    #[test]
    fn file_reference_resolves_against_lesson_dir() {
        let root = tmpdir("fileref");
        std::fs::create_dir_all(root.join("lessons")).unwrap();
        std::fs::create_dir_all(root.join("library")).unwrap();
        std::fs::write(root.join("library/p.json"), CLAVE).unwrap();
        let lesson_path = root.join("lessons/x.md");
        std::fs::write(
            &lesson_path,
            block(r#"{ "label": "B", "file": "../library/p.json", "cycles": 4, "hidden": true }"#),
        )
        .unwrap();
        let text = std::fs::read_to_string(&lesson_path).unwrap();
        let (l, d) = parse_str(&text, lesson_path.parent().unwrap());
        assert!(d.errors.is_empty(), "{:?}", d.errors);
        let e = l.unwrap().blocks().next().unwrap().clone();
        assert_eq!(e.label, "B");
        assert_eq!(e.cycles, Some(4));
        assert!(e.hidden);
        assert_eq!(e.tempo, 90.0);
        assert_eq!(e.voices[0].onsets.len(), 5);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn voices_override_replaces_wholesale() {
        let root = tmpdir("voices");
        std::fs::write(root.join("p.json"), CLAVE).unwrap();
        let text = block(
            r#"{ "file": "p.json", "voices": [
                { "name": "bell", "instrument": "bell", "pulses": 12, "onsets": [0] } ] }"#,
        );
        let l = parse_str(&text, &root).0.unwrap();
        let e = l.blocks().next().unwrap();
        assert_eq!(e.voices.len(), 1);
        assert_eq!(e.voices[0].name, "bell");
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn countdown_without_cycles_is_an_error_through_a_file_override() {
        let root = tmpdir("countdown");
        std::fs::write(root.join("p.json"), CLAVE).unwrap();
        let text = block(r#"{ "file": "p.json", "cycle_count": "countdown" }"#);
        let (l, d) = parse_str(&text, &root);
        assert!(l.is_none());
        assert_eq!(
            d.errors,
            ["block 1 \"A\": cycle_count: \"countdown\" needs cycles (a looping block never ends)"]
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn missing_file_is_an_error() {
        let (l, d) = parse_str(
            &block(r#"{ "label": "Z", "file": "../library/nope.json" }"#),
            Path::new("/nonexistent"),
        );
        assert!(l.is_none());
        assert_eq!(d.errors.len(), 1);
        assert!(d.errors[0].starts_with("block 1 \"Z\": file \"../library/nope.json\""));
    }

    /// A four-cycle son clave block labelled `label`, with `extra` keys.
    fn clave_block(label: &str, extra: &str) -> String {
        block(&format!(
            r#"{{ "label": "{label}", "tempo": 90, "beats": 4, "cycles": 4{extra}, "voices": [
            {{ "name": "clave", "instrument": "clave", "pulses": 16, "onsets": [0, 3, 6] }} ] }}"#
        ))
    }

    #[test]
    fn attacca_on_the_last_block_is_an_error() {
        let text = format!(
            "{}{}",
            clave_block("A", ""),
            clave_block("B", r#", "next": "attacca""#)
        );
        let (l, d) = parse_str(&text, Path::new("."));
        assert!(l.is_none());
        assert_eq!(
            d.errors,
            ["block 2 \"B\": next: \"attacca\" but no block follows"]
        );
    }

    #[test]
    fn attacca_without_cycles_is_an_error() {
        let looping = block(
            r#"{ "label": "A", "tempo": 90, "beats": 4, "next": "attacca", "voices": [
            { "name": "clave", "instrument": "clave", "pulses": 16, "onsets": [0] } ] }"#,
        );
        let text = format!("{looping}{}", clave_block("B", ""));
        let (l, d) = parse_str(&text, Path::new("."));
        assert!(l.is_none());
        assert_eq!(
            d.errors,
            ["block 1 \"A\": next: \"attacca\" needs cycles (a looping block never ends)"]
        );
    }

    #[test]
    fn attacca_chain_validates() {
        let text = format!(
            "{}between\n{}{}",
            clave_block("A", r#", "next": "attacca""#),
            clave_block("B", r#", "next": "attacca", "count_in": 2"#),
            clave_block("C", r#", "next": "stop""#)
        );
        let (l, d) = parse_str(&text, Path::new("."));
        assert!(d.errors.is_empty(), "{:?}", d.errors);
        let nexts: Vec<Next> = l.unwrap().blocks().map(|e| e.next).collect();
        assert_eq!(nexts, [Next::Attacca, Next::Attacca, Next::Stop]);
    }

    #[test]
    fn duplicate_labels() {
        let text = format!("{}text\n{}", block(CLAVE), block(CLAVE));
        let (l, d) = parse_str(&text, Path::new("."));
        assert!(l.is_none());
        assert_eq!(d.errors.len(), 1);
        assert!(d.errors[0].contains("block 2 \"A\""));
        assert!(d.errors[0].contains("block 1"));
    }

    #[test]
    fn duplicate_labels_numbered_by_block_after_a_bad_block() {
        let text = format!("{}{}{}", block("{ not json"), block(CLAVE), block(CLAVE));
        let (_, d) = parse_str(&text, Path::new("."));
        assert_eq!(d.errors.len(), 2, "{:?}", d.errors);
        assert_eq!(
            d.errors[1],
            "block 3 \"A\": duplicate label, also used by block 2"
        );
    }

    #[test]
    fn numeric_label_must_be_its_own_block_number() {
        let as_label = |l: &str| CLAVE.replace("\"A\"", &format!("\"{l}\""));
        let text = format!("{}{}", block(&as_label("2")), block(&as_label("B")));
        let (l, d) = parse_str(&text, Path::new("."));
        assert!(l.is_none());
        assert_eq!(
            d.errors,
            vec![
                "block 1 \"2\": a numeric label must be the block's own number (1); \
                 `djembot play 2` means block 2"
            ]
        );
        let text = format!("{}{}", block(&as_label("1")), block(&as_label("2")));
        let l = parse_str(&text, Path::new(".")).0.unwrap();
        assert_eq!(l.find("2"), Some(1));
    }

    #[test]
    fn unclosed_fence() {
        let text = format!("{}intro\n```djembot\n{CLAVE}\n", block(CLAVE));
        let (l, d) = parse_str(&text, Path::new("."));
        assert!(l.is_none());
        assert_eq!(d.errors.len(), 1);
        assert!(
            d.errors[0].starts_with("block 2: unclosed"),
            "{:?}",
            d.errors
        );
    }

    #[test]
    fn invalid_json_names_block() {
        let (l, d) = parse_str(
            &format!("{}{}", block(CLAVE), block("{ nope")),
            Path::new("."),
        );
        assert!(l.is_none());
        assert!(
            d.errors[0].starts_with("block 2 (line 5): invalid JSON"),
            "{:?}",
            d.errors
        );
    }

    #[test]
    fn exercise_errors_carry_block_and_label() {
        let bad = CLAVE.replace("12]", "16]");
        let (_, d) = parse_str(
            &format!(
                "{}{}",
                block(CLAVE.replace("\"A\"", "\"X\"").as_str()),
                block(&bad)
            ),
            Path::new("."),
        );
        assert_eq!(d.errors.len(), 1, "{:?}", d.errors);
        assert!(
            d.errors[0].starts_with("block 2 \"A\": voice 1: onsets[4]"),
            "{:?}",
            d.errors
        );
    }

    #[test]
    fn default_labels() {
        let untitled = CLAVE.replace(r#""label": "A","#, "");
        let titled = CLAVE.replace(r#""label": "A","#, r#""title": "Son","#);
        let l = parse_str(
            &format!("{}{}", block(&titled), block(&untitled)),
            Path::new("."),
        )
        .0
        .unwrap();
        assert_eq!(l.labels(), vec!["Son", "block 2"]);
        assert_eq!(l.find("Son"), Some(0));
        assert_eq!(l.find("2"), Some(1));
        assert_eq!(l.find("block 2"), Some(1));
        assert_eq!(l.find("3"), None);
        assert_eq!(l.find("0"), None);
    }

    #[test]
    fn text_only_lesson_is_valid() {
        let l = parse_str("# Just words\n\nNo rhythm today.\n", Path::new("."))
            .0
            .unwrap();
        assert_eq!(l.blocks().count(), 0);
        assert_eq!(l.segments.len(), 1);
    }

    #[test]
    fn parse_object_rejects_non_objects() {
        let mut d = Diagnostics::default();
        assert!(parse_object("[1]", "ctx: ", &mut d).is_none());
        assert_eq!(d.errors, vec!["ctx: not a JSON object"]);
        let mut d = Diagnostics::default();
        assert!(parse_object("{", "", &mut d).is_none());
        assert_eq!(d.errors.len(), 1);
        assert!(d.errors[0].starts_with("invalid JSON: "), "{:?}", d.errors);
        let mut d = Diagnostics::default();
        let m = parse_object(r#"{ "tempo": 90 }"#, "", &mut d).unwrap();
        assert_eq!(m["tempo"], 90);
        assert!(d.errors.is_empty());
    }

    #[test]
    fn crlf_fences() {
        let text = format!("```djembot\r\n{CLAVE}\r\n```\r\n");
        assert_eq!(
            parse_str(&text, Path::new(".")).0.unwrap().blocks().count(),
            1
        );
    }
}
