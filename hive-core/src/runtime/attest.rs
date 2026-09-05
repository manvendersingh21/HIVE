//! §9.4 — evidence over signals, applied to the verifier.
//!
//! The protocol's core bet is that verification is mechanical, not social. That bet was
//! tested for real: across the live runs, two different CLIs narrated successful file
//! creation that had not happened, one wrote its file somewhere nobody was looking, and
//! every one of those calls exited 0 (`docs/findings/adapter-edge.md`, findings 8–10).
//! The rule that caught all three is that an `accept` may not rest on an agent's word.
//!
//! So the verifying agent produces a *record* — named checks with details — and this
//! module produces *measurements*, taken by this process from the bytes on disk. An
//! accept goes on the wire only if at least one claimed passing check is corroborated
//! by a measurement, and none is contradicted by one.
//!
//! **The one soft edge, stated plainly:** matching a free-text check name to a
//! measurement is done by keyword. A verifier that names a check "everything looks
//! good" produces a claim this module can neither support nor refute, and such claims
//! are counted as `unmatched` — never as support. That asymmetry is the whole design:
//! an unrecognized claim can never help an accept, only fail to save it.

use std::path::Path;

use hacp::v2::contract::Verdict;
use hacp::v2::{canon, Artifact, Check};

/// What this process measured, from the file itself.
#[derive(Debug, Clone)]
pub struct Facts {
    pub bytes: Vec<u8>,
    pub digest: String,
    pub size: u64,
    /// Newline count. A single-line file conventionally ends in exactly one `\n`.
    pub newlines: usize,
    pub non_empty: bool,
}

impl Facts {
    /// Read and measure. Nothing here consults a claim.
    pub async fn measure(path: &Path) -> anyhow::Result<Self> {
        let bytes = tokio::fs::read(path)
            .await
            .map_err(|e| anyhow::anyhow!("cannot measure {}: {e}", path.display()))?;
        // `digest_canonical` hashes a `&str`. Lossy conversion would substitute
        // replacement characters and then hash *those* — producing a confident digest of
        // something that is not the file. Non-UTF-8 content is refused instead; the
        // artifacts this runtime negotiates are text, and a binary edge is a feature to
        // add deliberately, not to discover through a wrong hash.
        let text = std::str::from_utf8(&bytes).map_err(|e| {
            anyhow::anyhow!(
                "{} is not valid UTF-8 ({e}); this runtime measures text artifacts only",
                path.display()
            )
        })?;
        Ok(Self {
            digest: canon::digest_canonical(text),
            size: bytes.len() as u64,
            newlines: bytes.iter().filter(|b| **b == b'\n').count(),
            non_empty: !bytes.iter().all(|b| b.is_ascii_whitespace()),
            bytes,
        })
    }

    /// The content as text, for showing a verifier what it is judging.
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.bytes).to_string()
    }
}

/// How the verifier's claims stood up to measurement.
#[derive(Debug, Default, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Corroboration {
    /// Claimed-passing checks a measurement independently confirms.
    pub backed: Vec<String>,
    /// Claimed-passing checks a measurement refutes. Any of these is fatal.
    pub contradicted: Vec<String>,
    /// Claims with no measurement to compare against. Never support.
    pub unmatched: Vec<String>,
}

/// Compare a verifier's claimed-passing checks against measured facts.
///
/// `one_line` comes from the frozen terms, not from the verifier: whether a single line
/// was required is a property of the contract, and letting the verifier assert it would
/// let the judged party choose the standard.
pub fn corroborate(
    checks: &[Check],
    facts: &Facts,
    manifest: &Artifact,
    one_line: bool,
) -> Corroboration {
    let mut c = Corroboration::default();
    for check in checks.iter().filter(|c| c.passed) {
        let name = check.name.to_lowercase();
        let measured = if name.contains("digest") || name.contains("sha") || name.contains("hash") {
            Some(facts.digest == manifest.digest)
        } else if name.contains("line") {
            Some(if one_line { facts.newlines == 1 } else { facts.newlines >= 1 })
        } else if name.contains("size") || name.contains("byte") {
            Some(facts.size == manifest.size)
        } else if name.contains("empty") || name.contains("content") || name.contains("word") || name.contains("exist") {
            Some(facts.non_empty)
        } else {
            None
        };
        match measured {
            Some(true) => c.backed.push(check.name.clone()),
            Some(false) => c.contradicted.push(check.name.clone()),
            None => c.unmatched.push(check.name.clone()),
        }
    }
    c
}

/// The gate: may this verdict go on the wire?
///
/// Only `Accept` is gated. A `Reject` or `Rework` costs nothing to believe — it asks
/// for more work, and the failure mode this rule exists to stop is a run that settles
/// on nothing.
pub fn gate(verdict: &Verdict, c: &Corroboration) -> anyhow::Result<()> {
    if !matches!(verdict, Verdict::Accept) {
        return Ok(());
    }
    if !c.contradicted.is_empty() {
        anyhow::bail!(
            "refusing to accept: the verifier claims these checks passed, and measurement \
             says otherwise: {} (§9.4)",
            c.contradicted.join(", ")
        );
    }
    if c.backed.is_empty() {
        anyhow::bail!(
            "refusing to accept: no claimed check survives independent measurement \
             (unmatched claims: {}) — §9.4, evidence over signals",
            if c.unmatched.is_empty() { "none".into() } else { c.unmatched.join(", ") }
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use hacp::v2::Visibility;

    fn facts(content: &str) -> Facts {
        let bytes = content.as_bytes().to_vec();
        Facts {
            digest: canon::digest_canonical(content),
            size: bytes.len() as u64,
            newlines: bytes.iter().filter(|b| **b == b'\n').count(),
            non_empty: !bytes.iter().all(|b| b.is_ascii_whitespace()),
            bytes,
        }
    }

    fn manifest(f: &Facts) -> Artifact {
        Artifact {
            artifact_id: format!("urn:hacp:artifact:{}", uuid::Uuid::new_v4()),
            media_type: "text/plain".into(),
            digest: f.digest.clone(),
            size: f.size,
            producer: "urn:hacp:agent:wrk-test".into(),
            task_id: "t-1".into(),
            contract_id: "c-1".into(),
            contract_revision: "a".repeat(64),
            derived_from: vec![],
            location: "wrk/status.txt".into(),
            visibility: Visibility::Participants,
        }
    }

    fn check(name: &str, passed: bool) -> Check {
        Check { name: name.into(), passed, detail: "measured".into() }
    }

    #[test]
    fn a_true_claim_is_backed() {
        let f = facts("all done ready\n");
        let m = manifest(&f);
        let c = corroborate(&[check("sha256 matches", true)], &f, &m, true);
        assert_eq!(c.backed, vec!["sha256 matches"]);
        gate(&Verdict::Accept, &c).unwrap();
    }

    #[test]
    fn a_false_claim_is_contradicted_and_blocks_the_accept() {
        let f = facts("all done ready\n");
        let mut m = manifest(&f);
        m.digest = "b".repeat(64); // the submitted manifest does not describe the file
        let c = corroborate(&[check("digest verified", true)], &f, &m, true);
        assert_eq!(c.contradicted, vec!["digest verified"]);
        let e = gate(&Verdict::Accept, &c).unwrap_err().to_string();
        assert!(e.contains("measurement says otherwise"), "{e}");
    }

    #[test]
    fn an_accept_backed_only_by_unrecognized_claims_is_refused() {
        // Finding 8-10 in one assertion: an agent that says "I did it" convincingly
        // must not be able to settle a contract.
        let f = facts("ready\n");
        let m = manifest(&f);
        let c = corroborate(&[check("everything looks good to me", true)], &f, &m, true);
        assert!(c.backed.is_empty());
        assert_eq!(c.unmatched, vec!["everything looks good to me"]);
        let e = gate(&Verdict::Accept, &c).unwrap_err().to_string();
        assert!(e.contains("no claimed check survives"), "{e}");
    }

    #[test]
    fn failing_claims_are_not_counted_at_all() {
        let f = facts("ready\n");
        let m = manifest(&f);
        let c = corroborate(&[check("digest", false)], &f, &m, true);
        assert_eq!(c, Corroboration::default());
    }

    #[test]
    fn the_line_standard_comes_from_the_contract_not_the_verifier() {
        let f = facts("one\ntwo\n");
        let m = manifest(&f);
        // Under a one-line contract this claim is false...
        let strict = corroborate(&[check("is one line", true)], &f, &m, true);
        assert_eq!(strict.contradicted, vec!["is one line"]);
        // ...and under terms that did not require it, it is merely satisfied.
        let loose = corroborate(&[check("has lines", true)], &f, &m, false);
        assert_eq!(loose.backed, vec!["has lines"]);
    }

    #[test]
    fn rework_and_reject_are_not_gated() {
        // These ask for more work. The rule exists to stop a run settling on nothing,
        // not to stop it admitting failure.
        let c = Corroboration::default();
        gate(&Verdict::Reject, &c).unwrap();
        gate(&Verdict::Rework { scope: "redo it".into() }, &c).unwrap();
    }

    #[tokio::test]
    async fn measuring_a_missing_file_says_which_file() {
        let s = crate::runtime::Scratch::new("attest");
        let e = Facts::measure(&s.join("nope.txt")).await.unwrap_err().to_string();
        assert!(e.contains("cannot measure"), "{e}");
        assert!(e.contains("nope.txt"), "{e}");
    }

    #[tokio::test]
    async fn measurement_reads_the_bytes_not_the_claim() {
        let s = crate::runtime::Scratch::new("attest");
        let p = s.join("status.txt");
        tokio::fs::write(&p, "work complete, ready\n").await.unwrap();
        let f = Facts::measure(&p).await.unwrap();
        assert_eq!(f.size, 21);
        assert_eq!(f.newlines, 1);
        assert!(f.non_empty);
        assert_eq!(f.digest, canon::digest_canonical("work complete, ready\n"));
    }

    #[tokio::test]
    async fn a_non_utf8_artifact_is_refused_rather_than_hashed_lossily() {
        let s = crate::runtime::Scratch::new("attest");
        let p = s.join("bin.dat");
        tokio::fs::write(&p, [0xff, 0xfe, 0x00]).await.unwrap();
        let e = Facts::measure(&p).await.unwrap_err().to_string();
        assert!(e.contains("not valid UTF-8"), "{e}");
    }

    #[tokio::test]
    async fn a_whitespace_only_file_is_not_non_empty() {
        let s = crate::runtime::Scratch::new("attest");
        let p = s.join("blank.txt");
        tokio::fs::write(&p, "   \n\t\n").await.unwrap();
        let f = Facts::measure(&p).await.unwrap();
        assert!(!f.non_empty, "whitespace is not content");
    }
}
