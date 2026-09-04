//! The golden-transcript harness (`spec/HACP-2.0-draft.md` §14).
//!
//! Shared by the v2 integration tests. A transcript is the ordered sequence of
//! envelope frames exchanged between exactly two parties; a *golden* transcript is
//! one committed under `tests/golden/` that implementations replay and compare.
//!
//! Comparison is over **canonical form** (§5.1) with named *volatile fields*
//! normalized: values that are legitimately fresh on every run — `message_id`,
//! `timestamp` — are replaced by stable occurrence ordinals (`<<message_id#3>>`)
//! so two honest runs of the same lifecycle compare byte-identical while any
//! semantic drift still moves the bytes and fails the comparison.

// Linked into several test binaries; not every one uses every helper yet (the
// golden functions wake up with the W4/W6 lifecycles).
#![allow(dead_code)]

use hacp::v2::canon::{canonical_json, CanonError};
use serde::Serialize;
use serde_json::Value;

/// Which way a frame travelled. Names the two bilateral parties, never a vendor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dir {
    AToB,
    BToA,
}

impl Dir {
    pub fn as_str(self) -> &'static str {
        match self {
            Dir::AToB => "a>b",
            Dir::BToA => "b>a",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Frame {
    pub dir: Dir,
    pub envelope: Value,
}

#[derive(Debug, Clone, Default)]
pub struct Transcript {
    frames: Vec<Frame>,
}

impl Transcript {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn record(&mut self, dir: Dir, envelope: &impl Serialize) {
        let value = serde_json::to_value(envelope)
            .expect("transcript frames must be serializable values");
        self.record_value(dir, value);
    }

    pub fn record_value(&mut self, dir: Dir, envelope: Value) {
        self.frames.push(Frame { dir, envelope });
    }

    pub fn len(&self) -> usize {
        self.frames.len()
    }

    pub fn is_empty(&self) -> bool {
        self.frames.is_empty()
    }

    /// Direct access for tests that need to mutate a frame before comparing.
    pub fn frames_mut(&mut self) -> &mut [Frame] {
        &mut self.frames
    }

    /// Render to comparable bytes: each frame as canonical JSON, one per line,
    /// with the named volatile top-level fields replaced by occurrence ordinals.
    pub fn render(&self, volatile: &[&str]) -> Result<String, CanonError> {
        // First distinct value of each volatile field, in frame order, becomes 1;
        // repeats keep their ordinal. Fresh ids therefore normalize away while a
        // *semantic* use of the field (an in_reply_to that changes which message
        // is answered) is only normalized where it was declared volatile.
        let mut ordinals: std::collections::BTreeMap<(&str, String), u64> =
            std::collections::BTreeMap::new();
        let mut lines = Vec::with_capacity(self.frames.len());
        for frame in &self.frames {
            let mut envelope = frame.envelope.clone();
            if let Value::Object(map) = &mut envelope {
                for field in volatile {
                    if let Some(value) = map.get(*field) {
                        let key = (*field, canonical_json(value)?);
                        // Ordinals count per field: the third distinct message_id
                        // is #3 regardless of how many timestamps were seen.
                        let next = ordinals
                            .keys()
                            .filter(|(f, _)| *f == *field)
                            .count() as u64
                            + 1;
                        let n = *ordinals.entry(key).or_insert(next);
                        map.insert(
                            field.to_string(),
                            Value::String(format!("<<{}#{}>>", field, n)),
                        );
                    }
                }
            }
            let frame_value = serde_json::json!({
                "dir": frame.dir.as_str(),
                "envelope": envelope,
            });
            lines.push(canonical_json(&frame_value)?);
        }
        Ok(lines.join("\n"))
    }

    /// Save raw frames (no normalization — goldens keep their real ids for an
    /// independent implementer to inspect) as canonical JSONL.
    pub fn save(&self, path: &std::path::Path) -> std::result::Result<(), String> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("creating {}: {e}", parent.display()))?;
        }
        let mut out = String::new();
        for frame in &self.frames {
            let frame_value = serde_json::json!({
                "dir": frame.dir.as_str(),
                "envelope": frame.envelope,
            });
            out.push_str(&canonical_json(&frame_value).map_err(|e| e.to_string())?);
            out.push('\n');
        }
        std::fs::write(path, out).map_err(|e| format!("writing {}: {e}", path.display()))
    }

    pub fn load(path: &std::path::Path) -> std::result::Result<Self, String> {
        let raw = std::fs::read_to_string(path).map_err(|e| format!("reading {}: {e}", path.display()))?;
        let mut frames = Vec::new();
        for (i, line) in raw.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            let value: Value = serde_json::from_str(line)
                .map_err(|e| format!("{} line {}: not JSON: {e}", path.display(), i + 1))?;
            let dir = match value.get("dir").and_then(Value::as_str) {
                Some("a>b") => Dir::AToB,
                Some("b>a") => Dir::BToA,
                other => {
                    return Err(format!(
                        "{} line {}: unknown dir {other:?}",
                        path.display(),
                        i + 1
                    ))
                }
            };
            let envelope = value
                .get("envelope")
                .cloned()
                .ok_or_else(|| format!("{} line {}: no envelope", path.display(), i + 1))?;
            frames.push(Frame { dir, envelope });
        }
        Ok(Self { frames })
    }

    /// Compare against another transcript under normalization, reporting the
    /// first divergence by frame index with a byte window around it.
    pub fn compare(
        &self,
        other: &Transcript,
        volatile: &[&str],
    ) -> std::result::Result<(), String> {
        let mine = self.render(volatile).map_err(|e| e.to_string())?;
        let theirs = other.render(volatile).map_err(|e| e.to_string())?;
        if mine == theirs {
            return Ok(());
        }
        let my_lines: Vec<&str> = mine.lines().collect();
        let their_lines: Vec<&str> = theirs.lines().collect();
        let index = my_lines
            .iter()
            .zip(their_lines.iter())
            .position(|(a, b)| a != b)
            .unwrap_or_else(|| my_lines.len().min(their_lines.len()));
        let detail = match (my_lines.get(index), their_lines.get(index)) {
            (Some(a), Some(b)) => format!("frame {index}:\n  recorded: {a}\n  other:    {b}"),
            (Some(a), None) => format!("frame {index}: extra recorded: {a}"),
            (None, Some(b)) => format!("frame {index}: missing recorded, other has: {b}"),
            (None, None) => unreachable!("equal renders were handled above"),
        };
        Err(format!("transcripts diverge at {detail}"))
    }
}

/// Path of a committed golden transcript by name.
pub fn golden_path(name: &str) -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/golden")
        .join(format!("{name}.jsonl"))
}

/// Record a golden when `HACP_RECORD_GOLDEN` is set (the writer mode); a no-op
/// otherwise, so the same test binary replays in CI and records on demand.
pub fn maybe_record_golden(name: &str, transcript: &Transcript) {
    if std::env::var_os("HACP_RECORD_GOLDEN").is_some() {
        let path = golden_path(name);
        transcript
            .save(&path)
            .unwrap_or_else(|e| panic!("recording golden {name}: {e}"));
    }
}

/// Assert a transcript matches its committed golden, or say how to record one.
pub fn assert_matches_golden(name: &str, transcript: &Transcript, volatile: &[&str]) {
    let path = golden_path(name);
    if !path.exists() {
        panic!(
            "golden {name} is not recorded at {}; run with HACP_RECORD_GOLDEN=1 once",
            path.display()
        );
    }
    let golden = Transcript::load(&path).unwrap_or_else(|e| panic!("loading golden {name}: {e}"));
    transcript
        .compare(&golden, volatile)
        .unwrap_or_else(|e| panic!("golden {name} mismatch: {e}"));
}
