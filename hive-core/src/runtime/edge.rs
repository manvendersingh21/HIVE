//! The file edge (§12.1) and the transcript.
//!
//! Both adapters run inside this one process, so the edge could have been a variable.
//! It is a pair of directories instead, for three reasons that all cost something to
//! learn: a run is auditable afterwards without a debugger; the same code moves to two
//! machines by changing where the directories are; and a message that is a file cannot
//! quietly acquire a field on the way across, which is the failure a shared in-memory
//! struct invites.
//!
//! Each side writes only to its own outbox and reads only from the other's. There is no
//! shared mutable state between the two sides at all — that is what makes the transcript
//! agreement check at the end of a run meaningful rather than tautological.

use std::path::PathBuf;

use hacp::v2::{canon, Envelope};
use serde_json::Value;

/// Fields that legitimately differ between two independently minted views of the same
/// message, and so are stripped before transcripts are compared.
const VOLATILE: &[&str] = &["message_id", "timestamp"];

/// One message as one side saw it, and which way it went.
#[derive(Debug, Clone, PartialEq)]
pub struct Frame {
    /// `"a>b"` or `"b>a"` — direction, not authorship of this record.
    pub dir: String,
    pub envelope: Envelope,
}

/// One participant's adapter: an outbox, the peer's outbox as its inbox, and the
/// frames it witnessed.
pub struct Side {
    /// `"a"` or `"b"`.
    pub label: &'static str,
    pub urn: String,
    out_dir: PathBuf,
    in_dir: PathBuf,
    counter: u32,
    pub frames: Vec<Frame>,
}

impl Side {
    pub fn new(label: &'static str, urn: impl Into<String>, out_dir: PathBuf, in_dir: PathBuf) -> Self {
        Self {
            label,
            urn: urn.into(),
            out_dir,
            in_dir,
            counter: 0,
            frames: Vec::new(),
        }
    }

    fn peer_label(&self) -> &'static str {
        if self.label == "a" {
            "b"
        } else {
            "a"
        }
    }

    /// Mint, validate, write, and record one outbound message.
    ///
    /// Validation happens before the write, not after: an envelope that fails §5 is a
    /// bug in this runtime, and writing it first would hand the peer a malformed file
    /// and then panic on our own side, leaving the edge in a state no restart can read.
    pub async fn emit(
        &mut self,
        session_id: &str,
        to: &str,
        kind: &str,
        body: Value,
    ) -> anyhow::Result<Envelope> {
        let env = Envelope::new(session_id, self.urn.clone(), to, kind, body);
        env.validate()
            .map_err(|e| anyhow::anyhow!("refusing to emit an invalid {kind}: {e}"))?;
        // A round-trip through the wire form: the typed struct is what makes this build
        // schema-correct by construction, and this proves the bytes survive it. It is
        // not a JSON-Schema validation run, and this comment exists so nobody later
        // reads it as one — the committed schemas are enforced by `hacp`'s own drift
        // gate, against the same types.
        let bytes = serde_json::to_vec(&env)?;
        let round: Envelope = serde_json::from_slice(&bytes)?;
        anyhow::ensure!(round == env, "envelope did not survive its own wire form");

        self.counter += 1;
        tokio::fs::create_dir_all(&self.out_dir).await?;
        let name = format!("{:03}-{kind}.json", self.counter);
        tokio::fs::write(self.out_dir.join(name), &bytes).await?;

        self.frames.push(Frame {
            dir: format!("{}>{}", self.label, self.peer_label()),
            envelope: env.clone(),
        });
        Ok(env)
    }

    /// Accept one inbound message: validate it, check it is actually addressed here,
    /// and record it.
    pub fn receive(&mut self, env: Envelope) -> anyhow::Result<Envelope> {
        env.validate()
            .map_err(|e| anyhow::anyhow!("{} received an invalid envelope: {e}", self.label))?;
        anyhow::ensure!(
            env.to == self.urn,
            "envelope addressed to {} arrived at {}",
            env.to,
            self.urn
        );
        self.frames.push(Frame {
            dir: format!("{}>{}", self.peer_label(), self.label),
            envelope: env.clone(),
        });
        Ok(env)
    }

    /// The most recent message in this side's inbox.
    ///
    /// Ordering is by filename, and filenames carry the emitter's own counter — so
    /// "most recent" means the peer's latest send, not the filesystem's opinion about
    /// modification times, which is not reliable at sub-second resolution.
    pub async fn read_latest(&self) -> anyhow::Result<Envelope> {
        let mut names: Vec<String> = Vec::new();
        let mut rd = tokio::fs::read_dir(&self.in_dir).await.map_err(|e| {
            anyhow::anyhow!("cannot read inbox {}: {e}", self.in_dir.display())
        })?;
        while let Some(entry) = rd.next_entry().await? {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.ends_with(".json") {
                names.push(name);
            }
        }
        names.sort();
        let last = names
            .last()
            .ok_or_else(|| anyhow::anyhow!("inbox {} is empty", self.in_dir.display()))?;
        let bytes = tokio::fs::read(self.in_dir.join(last)).await?;
        Ok(serde_json::from_slice(&bytes)?)
    }
}

/// One transcript line: a frame with its volatile fields removed.
pub fn strip_volatile(frame: &Frame) -> anyhow::Result<Value> {
    let mut env = serde_json::to_value(&frame.envelope)?;
    if let Some(map) = env.as_object_mut() {
        for k in VOLATILE {
            map.remove(*k);
        }
    }
    Ok(serde_json::json!({ "dir": frame.dir, "envelope": env }))
}

/// Do both sides agree, frame for frame, about what was exchanged?
///
/// This is the cheapest strong check in the whole run. The two sides share no state:
/// one wrote each message and the other parsed it back off disk. If their views
/// canonicalize identically, then every field survived serialization, addressing, and
/// ordering — and if they do not, the first differing frame is named, because
/// "transcripts differ" is not a diagnosis.
pub fn transcripts_agree(a: &[Frame], b: &[Frame]) -> anyhow::Result<()> {
    let render = |fs: &[Frame]| -> anyhow::Result<Vec<String>> {
        fs.iter()
            .map(|f| Ok(canon::canonical_json(&strip_volatile(f)?)?))
            .collect()
    };
    let (av, bv) = (render(a)?, render(b)?);
    for (i, (fa, fb)) in av.iter().zip(bv.iter()).enumerate() {
        if fa != fb {
            anyhow::bail!("transcript divergence at frame {i}:\n  a: {fa}\n  b: {fb}");
        }
    }
    anyhow::ensure!(
        av.len() == bv.len(),
        "transcript length divergence: a saw {} frames, b saw {}",
        av.len(),
        bv.len()
    );
    Ok(())
}

/// Render one side's frames as JSONL, one canonical frame per line.
pub fn transcript_lines(frames: &[Frame]) -> anyhow::Result<String> {
    let mut out = String::new();
    for f in frames {
        out.push_str(&canon::canonical_json(&strip_volatile(f)?)?);
        out.push('\n');
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use hacp::v2::kinds;
    use serde_json::json;

    const A: &str = "urn:hacp:agent:sup-test";
    const B: &str = "urn:hacp:agent:wrk-test";

    fn dirs() -> (crate::runtime::Scratch, PathBuf, PathBuf) {
        let t = crate::runtime::Scratch::new("edge");
        let a = t.join("a-out");
        let b = t.join("b-out");
        (t, a, b)
    }

    fn sides(a_out: PathBuf, b_out: PathBuf) -> (Side, Side) {
        (
            Side::new("a", A, a_out.clone(), b_out.clone()),
            Side::new("b", B, b_out, a_out),
        )
    }

    #[tokio::test]
    async fn a_message_crosses_the_edge_and_both_views_agree() {
        let (_t, ao, bo) = dirs();
        let (mut a, mut b) = sides(ao, bo);
        a.emit("s-1", B, kinds::SESSION_OPEN, json!({"prospective": true}))
            .await
            .unwrap();
        let got = b.read_latest().await.unwrap();
        b.receive(got).unwrap();
        transcripts_agree(&a.frames, &b.frames).unwrap();
    }

    #[tokio::test]
    async fn a_message_addressed_elsewhere_is_refused() {
        let (_t, ao, bo) = dirs();
        let (mut a, mut b) = sides(ao, bo);
        a.emit("s-1", "urn:hacp:agent:someone-else", kinds::HEARTBEAT, json!({}))
            .await
            .unwrap();
        let got = b.read_latest().await.unwrap();
        let e = b.receive(got).unwrap_err().to_string();
        assert!(e.contains("arrived at"), "{e}");
    }

    #[tokio::test]
    async fn divergence_names_the_frame_that_differs() {
        let (_t, ao, bo) = dirs();
        let (mut a, mut b) = sides(ao, bo);
        a.emit("s-1", B, kinds::SESSION_OPEN, json!({"prospective": true}))
            .await
            .unwrap();
        let mut got = b.read_latest().await.unwrap();
        got.body = json!({"prospective": false}); // a message tampered with in flight
        b.receive(got).unwrap();
        let e = transcripts_agree(&a.frames, &b.frames).unwrap_err().to_string();
        assert!(e.contains("divergence at frame 0"), "{e}");
    }

    #[tokio::test]
    async fn a_length_difference_is_divergence_too() {
        // The zip above stops at the shorter side; without the explicit length check a
        // dropped final message would read as agreement.
        let (_t, ao, bo) = dirs();
        let (mut a, mut b) = sides(ao, bo);
        a.emit("s-1", B, kinds::SESSION_OPEN, json!({}))
            .await
            .unwrap();
        let e = transcripts_agree(&a.frames, &b.frames).unwrap_err().to_string();
        assert!(e.contains("length divergence"), "{e}");
    }

    #[tokio::test]
    async fn volatile_fields_do_not_count_as_divergence() {
        // Each side mints its own view; message ids and timestamps are expected to
        // differ and must not be mistaken for a wire disagreement.
        let (_t, ao, bo) = dirs();
        let (mut a, mut b) = sides(ao, bo);
        a.emit("s-1", B, kinds::SESSION_OPEN, json!({"prospective": true}))
            .await
            .unwrap();
        let mut got = b.read_latest().await.unwrap();
        got.message_id = "m-ffffffffffff".into();
        got.timestamp = "2020-01-01T00:00:00Z".into();
        b.receive(got).unwrap();
        transcripts_agree(&a.frames, &b.frames).unwrap();
    }

    #[tokio::test]
    async fn an_inbox_read_takes_the_peers_latest_by_its_own_counter() {
        let (_t, ao, bo) = dirs();
        let (mut a, b) = sides(ao, bo);
        for _ in 0..12 {
            a.emit("s-1", B, kinds::HEARTBEAT, json!({})).await.unwrap();
        }
        a.emit("s-1", B, kinds::SESSION_CLOSE, json!({"reason": "done"}))
            .await
            .unwrap();
        // 013 must sort after 012 — the zero padding is what makes that true.
        let got = b.read_latest().await.unwrap();
        assert_eq!(got.kind, kinds::SESSION_CLOSE);
    }

    #[tokio::test]
    async fn an_invalid_envelope_is_never_written() {
        let (_t, ao, bo) = dirs();
        let (mut a, _b) = sides(ao.clone(), bo);
        let e = a
            .emit("s-1", "not-a-urn", kinds::HEARTBEAT, json!({}))
            .await
            .unwrap_err()
            .to_string();
        assert!(e.contains("refusing to emit"), "{e}");
        assert!(!ao.exists() || std::fs::read_dir(&ao).unwrap().count() == 0);
    }
}
