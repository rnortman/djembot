//! The learner kit `djembot init` lays down: the instructor contract, the
//! journal skeleton, the library and the model lesson, embedded from
//! `starter/` at build time.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::Path;

/// `(path, content)` of the file `starter/<path>`, which lands at its own
/// path.
macro_rules! kit {
    ($path:literal) => {
        ($path, include_str!(concat!("../starter/", $path)))
    };
}

/// (target path under the learning directory, content). Every file under
/// `starter/` appears once; `starter/instructor.md` lands as `CLAUDE.md`,
/// every other file at its own path.
pub const FILES: &[(&str, &str)] = &[
    ("CLAUDE.md", include_str!("../starter/instructor.md")),
    // TODO(journal-log): the journal is appended to forever; sessions
    // belong in log/.
    kit!("JOURNAL.md"),
    kit!("library/bembe-bell.json"),
    kit!("library/four-against-three.json"),
    kit!("library/pulse-4.json"),
    kit!("library/rumba-clave-2-3.json"),
    kit!("library/rumba-clave-3-2.json"),
    kit!("library/six-eight-clave.json"),
    kit!("library/son-clave-2-3.json"),
    kit!("library/son-clave-3-2.json"),
    kit!("library/subdivision-16.json"),
    kit!("library/three-against-two.json"),
    kit!("library/twelve-vs-sixteen.json"),
    kit!("lessons/000-twelve-vs-sixteen.md"),
];

/// The last line of a successful `init`.
pub const NEXT: &str = "next: git init here, run `djembot gui` in a terminal in your graphical \
                        session, and start your agent in this directory.";

/// Writes every file of `FILES` under `dir` (created if absent) that does
/// not exist yet, and leaves every existing one alone; prints one line per
/// file to `out` — `wrote PATH`, `kept PATH (same as this version)` or
/// `kept PATH (differs from this version's template)` — then `NEXT`. Stops
/// at the first I/O error, naming the path.
pub fn init(dir: &Path, out: &mut impl Write) -> Result<(), String> {
    fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    for (target, content) in FILES {
        let path = dir.join(target);
        let fail = |e: io::Error| format!("{}: {e}", path.display());
        let line = match fs::read(&path) {
            Ok(existing) if existing == content.as_bytes() => {
                format!("kept {target} (same as this version)")
            }
            Ok(_) => format!("kept {target} (differs from this version's template)"),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                if let Some(parent) = path.parent() {
                    fs::create_dir_all(parent).map_err(fail)?;
                }
                let file = OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&path)
                    .map_err(fail)?;
                write_or_remove(&path, file, content).map_err(fail)?;
                format!("wrote {target}")
            }
            Err(e) => return Err(fail(e)),
        };
        writeln!(out, "{line}").map_err(|e| e.to_string())?;
    }
    writeln!(out, "{NEXT}").map_err(|e| e.to_string())
}

/// Writes `content` to `file`, just created at `path`. On failure removes
/// `path`, so the next `init` writes it again rather than keeping a partial
/// file as one that differs from the template.
fn write_or_remove(path: &Path, mut file: impl Write, content: &str) -> io::Result<()> {
    file.write_all(content.as_bytes()).inspect_err(|_| {
        let _ = fs::remove_file(path);
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Diagnostics;
    use crate::{Checked, Input, check};
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    fn starter_dir() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("starter")
    }

    /// Every file under `dir`, as paths relative to `root`.
    fn walk(root: &Path, dir: &Path, out: &mut Vec<PathBuf>) {
        for entry in fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                walk(root, &path, out);
            } else {
                out.push(path.strip_prefix(root).unwrap().to_path_buf());
            }
        }
    }

    /// (target, `starter/` source) of the one file that does not land at its
    /// own path.
    const CONTRACT: (&str, &str) = ("CLAUDE.md", "instructor.md");

    /// The `starter/` source of a target path.
    fn source(target: &str) -> PathBuf {
        starter_dir().join(if target == CONTRACT.0 {
            CONTRACT.1
        } else {
            target
        })
    }

    /// The target path of a `starter/` source.
    fn target(source: &str) -> &str {
        if source == CONTRACT.1 {
            CONTRACT.0
        } else {
            source
        }
    }

    #[test]
    fn the_embedded_files_are_exactly_the_starter_directory() {
        let root = starter_dir();
        let mut on_disk = Vec::new();
        walk(&root, &root, &mut on_disk);
        let on_disk: BTreeMap<String, String> = on_disk
            .into_iter()
            .map(|rel| {
                let rel = rel.to_str().unwrap();
                (
                    target(rel).to_string(),
                    fs::read_to_string(root.join(rel)).unwrap(),
                )
            })
            .collect();
        let embedded: BTreeMap<String, String> = FILES
            .iter()
            .map(|(t, c)| (t.to_string(), c.to_string()))
            .collect();
        assert_eq!(FILES.len(), embedded.len(), "a target appears twice");
        assert_eq!(
            on_disk.keys().collect::<Vec<_>>(),
            embedded.keys().collect::<Vec<_>>()
        );
        assert_eq!(on_disk, embedded);
    }

    fn assert_clean(path: &Path) -> Checked {
        let mut diag = Diagnostics::default();
        let checked = check(&Input::File(path.to_path_buf()), false, &mut diag);
        assert!(
            diag.errors.is_empty() && diag.warnings.is_empty(),
            "{}: {:?}",
            path.display(),
            diag
        );
        checked.unwrap()
    }

    #[test]
    fn every_embedded_lesson_and_exercise_validates() {
        let mut lessons = 0;
        let mut exercises = 0;
        for (target, _) in FILES {
            if target.starts_with("lessons/") {
                assert!(matches!(assert_clean(&source(target)), Checked::Lesson(_)));
                lessons += 1;
            } else if target.starts_with("library/") {
                assert!(matches!(
                    assert_clean(&source(target)),
                    Checked::Exercise(_)
                ));
                exercises += 1;
            }
        }
        assert_eq!((lessons, exercises), (1, 11));
    }

    #[test]
    fn model_lesson_resolves_library_references() {
        let Checked::Lesson(l) = assert_clean(&source("lessons/000-twelve-vs-sixteen.md")) else {
            panic!("not a lesson");
        };
        let blocks: Vec<_> = l.blocks().collect();
        assert_eq!(blocks.len(), 3);
        assert_eq!(blocks[0].voices[0].pulses, 12);
        assert_eq!(blocks[1].voices[0].pulses, 16);
        let both: Vec<u32> = blocks[2].voices.iter().map(|v| v.pulses).collect();
        assert_eq!(both, vec![12, 16, 4]);
    }

    #[test]
    fn init_names_the_path_it_cannot_write() {
        let dir = std::env::temp_dir().join(format!("djembot-starter-err-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("CLAUDE.md")).unwrap();
        let err = init(&dir, &mut Vec::new()).unwrap_err();
        assert!(
            err.starts_with(&format!("{}: ", dir.join("CLAUDE.md").display())),
            "{err}"
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    /// A writer that takes a few bytes, then fails as a full disk would.
    struct FailsAfter(usize);

    impl Write for FailsAfter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            if self.0 == 0 {
                return Err(io::Error::from(io::ErrorKind::StorageFull));
            }
            let n = buf.len().min(self.0);
            self.0 -= n;
            Ok(n)
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn a_failed_write_leaves_no_file_behind() {
        let dir = std::env::temp_dir().join(format!("djembot-starter-full-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("CLAUDE.md");
        fs::write(&path, "partial").unwrap();

        let err = write_or_remove(&path, FailsAfter(3), "the whole contract").unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::StorageFull);
        assert!(!path.exists());

        init(&dir, &mut Vec::new()).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), FILES[0].1);
        fs::remove_dir_all(&dir).unwrap();
    }
}
