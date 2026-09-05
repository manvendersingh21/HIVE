//! Acting on a human's answer to an incident.
//!
//! [`HumanDecision`] has had four variants in `hive-common` since Phase 1 and
//! no consumer: nothing could resume, abort, or amend a session the watchdog
//! had suspended. A suspended session was a dead end — the operator was told
//! how to attach and left to `kill -CONT` by hand. This module is the missing
//! half of the loop.
//!
//! # Why the decision is recorded before it is applied
//!
//! [`IncidentStore::resolve`] is a compare-and-swap: exactly one caller can
//! move an incident out of review. Recording first is what makes that
//! guarantee mean anything. If the desk acted first and recorded afterwards,
//! two operators with the review page open could both act — and *resume* and
//! *abort* racing on the same suspended process is precisely the outcome a
//! human was asked to prevent.
//!
//! The cost of that ordering is a window where the decision is durable but the
//! session has not moved: the SSH call fails after the row is written. That
//! window cannot be closed (there is no transaction spanning SQLite and a
//! remote tmux), so it is reported rather than hidden —
//! [`DecisionError::RecordedButNotApplied`] names the session and says the
//! incident is already resolved, so the operator knows to attach by hand and
//! knows not to expect a second click to work.

use async_trait::async_trait;
use hive_common::protocol::{HumanDecision, Incident};

use super::incidents::{IncidentStore, ResolveError};

/// The operations applying a decision needs from a session.
///
/// A trait rather than a bare [`crate::workers::ssh::SshWorker`] because the
/// interesting behaviour here is *ordering* — keys before continue, interrupt
/// before replacement — and ordering is exactly what cannot be tested against
/// a real remote host in a hermetic suite.
#[async_trait]
pub trait SessionControl: Send + Sync {
    /// `kill -CONT` the stopped process group.
    async fn resume(&self, session: &str) -> anyhow::Result<()>;
    /// End the session for good. The only place in this codebase where killing
    /// a supervised session is correct: a human has explicitly asked for it.
    async fn abort(&self, session: &str) -> anyhow::Result<()>;
    /// Type a line into the session's pane, as if at the keyboard.
    async fn send_line(&self, session: &str, line: &str) -> anyhow::Result<()>;
    /// Send the interrupt character to the pane.
    async fn interrupt(&self, session: &str) -> anyhow::Result<()>;
    /// Whether the session still exists.
    async fn is_live(&self, session: &str) -> bool;
}

/// What applying a decision actually did to the session.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Applied {
    /// The stopped process group was continued.
    Resumed,
    /// Continued, with a note typed into the pane first.
    ResumedWithNote,
    /// The running command was interrupted and a replacement typed in.
    ModifiedAndResumed,
    /// The session was killed.
    Aborted,
    /// The decision was recorded, but the session had already ended — there
    /// was nothing left to act on. Not an error: an agent finishing while a
    /// human deliberates is the common case, and the audit record still stands.
    SessionAlreadyGone,
}

#[derive(Debug, thiserror::Error)]
pub enum DecisionError {
    #[error(transparent)]
    Resolve(#[from] ResolveError),
    #[error(
        "incident '{id}' is recorded as reviewed, but session '{session}' could not be \
         {intent}: {source}. The decision stands and cannot be re-submitted — attach to \
         the session and finish by hand."
    )]
    RecordedButNotApplied {
        id: String,
        session: String,
        intent: &'static str,
        #[source]
        source: anyhow::Error,
    },
}

/// Pairs the incident log with the ability to act on a session.
pub struct ReviewDesk<C: SessionControl> {
    store: IncidentStore,
    control: C,
}

impl<C: SessionControl> ReviewDesk<C> {
    pub fn new(store: IncidentStore, control: C) -> Self {
        Self { store, control }
    }

    pub fn store(&self) -> &IncidentStore {
        &self.store
    }

    /// Record a human's answer, then carry it out.
    ///
    /// Returns the resolved incident and what the session did. See the module
    /// docs for why the two steps are in this order and what it costs.
    pub async fn decide(
        &self,
        id: &str,
        decision: &HumanDecision,
    ) -> Result<(Incident, Applied), DecisionError> {
        let incident = self.store.resolve(id, decision)?;
        let session = incident.tmux_session.clone();

        if !self.control.is_live(&session).await {
            tracing::info!(
                incident = id,
                session = %session,
                "decision recorded; session had already ended"
            );
            return Ok((incident, Applied::SessionAlreadyGone));
        }

        let applied = self
            .apply(&session, decision)
            .await
            .map_err(|(intent, source)| DecisionError::RecordedButNotApplied {
                id: id.to_string(),
                session: session.clone(),
                intent,
                source,
            })?;

        tracing::info!(incident = id, session = %session, ?applied, "incident resolved");
        Ok((incident, applied))
    }

    async fn apply(
        &self,
        session: &str,
        decision: &HumanDecision,
    ) -> Result<Applied, (&'static str, anyhow::Error)> {
        match decision {
            HumanDecision::Resume => {
                self.control.resume(session).await.map_err(|e| ("resumed", e))?;
                Ok(Applied::Resumed)
            }

            // The pane is written to *before* the process group is continued.
            // A stopped process cannot read, but the tty's line discipline
            // buffers the input regardless, so the note is waiting the instant
            // the process runs again. Continuing first would race: the command
            // could finish before the keys arrive.
            //
            // What a note can and cannot do: it lands as terminal input. A
            // session spawned by `spawn_tmux` runs one non-interactive command,
            // and a CLI agent that never reads stdin will never see it — it
            // stays visible in the pane for whoever attaches. The durable part
            // of "resume with note" is the note stored on the incident; typing
            // it in is best-effort on top of that, not the point of it.
            HumanDecision::ResumeWithNote(note) => {
                self.control
                    .send_line(session, note)
                    .await
                    .map_err(|e| ("sent the note", e))?;
                self.control.resume(session).await.map_err(|e| ("resumed", e))?;
                Ok(Applied::ResumedWithNote)
            }

            // "Instead of", per the variant's own documentation — so the
            // running command has to go before the replacement is typed. The
            // interrupt is queued to the tty while the group is stopped and
            // delivered on continue, which is the only ordering that reaches a
            // process that cannot currently be signalled through its terminal.
            //
            // Whether the replacement then runs depends on the session
            // surviving its command's death, which depends on how it was
            // spawned. The incident record is authoritative about what the
            // operator asked for; the pane is best-effort about carrying it out.
            HumanDecision::ModifyAndResume(command) => {
                self.control
                    .interrupt(session)
                    .await
                    .map_err(|e| ("interrupted", e))?;
                self.control.resume(session).await.map_err(|e| ("resumed", e))?;
                self.control
                    .send_line(session, command)
                    .await
                    .map_err(|e| ("given its replacement command", e))?;
                Ok(Applied::ModifiedAndResumed)
            }

            HumanDecision::Abort => {
                self.control.abort(session).await.map_err(|e| ("aborted", e))?;
                Ok(Applied::Aborted)
            }
        }
    }
}

/// The production control surface: tmux over SSH.
///
/// The impl lives here rather than beside [`crate::workers::ssh::SshWorker`]
/// so that the ordering rules above and the calls that implement them stay in
/// one file.
#[async_trait]
impl SessionControl for crate::workers::ssh::SshWorker {
    async fn resume(&self, session: &str) -> anyhow::Result<()> {
        self.resume_session(session).await
    }

    async fn abort(&self, session: &str) -> anyhow::Result<()> {
        // `=` stops tmux prefix-matching, so aborting `hive-1` can never take
        // down `hive-10`. The same footgun cost this codebase a working pause
        // for an entire phase (see `workers::ssh::pane_target`).
        self.run(&format!("tmux kill-session -t '={session}'")).await?;
        Ok(())
    }

    async fn send_line(&self, session: &str, line: &str) -> anyhow::Result<()> {
        self.send_keys(session, &[line, "Enter"]).await
    }

    async fn interrupt(&self, session: &str) -> anyhow::Result<()> {
        self.send_keys(session, &["C-c"]).await
    }

    async fn is_live(&self, session: &str) -> bool {
        self.session_exists(session).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::watchdog::incidents::new_incident;
    use hive_common::{SafetyAnalysis, SafetyCategory, Severity};
    use std::sync::{Arc, Mutex};

    /// Records the exact sequence of calls, so ordering — the whole reason
    /// [`SessionControl`] is a trait — is assertable.
    #[derive(Default)]
    struct FakeSession {
        calls: Arc<Mutex<Vec<String>>>,
        live: bool,
        fail_on: Option<&'static str>,
    }

    impl FakeSession {
        fn live() -> Self {
            Self { calls: Arc::default(), live: true, fail_on: None }
        }
        fn gone() -> Self {
            Self { calls: Arc::default(), live: false, fail_on: None }
        }
        fn failing(op: &'static str) -> Self {
            Self { calls: Arc::default(), live: true, fail_on: Some(op) }
        }
        fn note(&self, op: &str) -> anyhow::Result<()> {
            self.calls.lock().unwrap().push(op.to_string());
            match self.fail_on {
                Some(f) if op.starts_with(f) => anyhow::bail!("{f} failed"),
                _ => Ok(()),
            }
        }
    }

    #[async_trait]
    impl SessionControl for FakeSession {
        async fn resume(&self, _s: &str) -> anyhow::Result<()> {
            self.note("resume")
        }
        async fn abort(&self, _s: &str) -> anyhow::Result<()> {
            self.note("abort")
        }
        async fn send_line(&self, _s: &str, line: &str) -> anyhow::Result<()> {
            self.note(&format!("send:{line}"))
        }
        async fn interrupt(&self, _s: &str) -> anyhow::Result<()> {
            self.note("interrupt")
        }
        async fn is_live(&self, _s: &str) -> bool {
            self.live
        }
    }

    fn desk(control: FakeSession) -> (ReviewDesk<FakeSession>, String) {
        let store = IncidentStore::in_memory().unwrap();
        let incident = new_incident(
            "task-1",
            "cis-a6000",
            "hive-w1",
            SafetyAnalysis {
                is_safe: false,
                severity: Severity::Critical,
                category: Some(SafetyCategory::DestructiveCommand),
                reason: "Tier-1 rule 'destructive' matched: rm -rf /".into(),
                suggested_action: "Pause and review.".into(),
            },
            "rm -rf /",
        );
        store.record(&incident).unwrap();
        (ReviewDesk::new(store, control), incident.id)
    }

    #[tokio::test]
    async fn resume_continues_the_stopped_group() {
        let (desk, id) = desk(FakeSession::live());
        let (incident, applied) = desk.decide(&id, &HumanDecision::Resume).await.unwrap();

        assert_eq!(applied, Applied::Resumed);
        assert_eq!(
            incident.review_state,
            hive_common::protocol::IncidentReviewState::Resumed
        );
    }

    #[tokio::test]
    async fn abort_kills_the_session() {
        let (desk, id) = desk(FakeSession::live());
        let (incident, applied) = desk.decide(&id, &HumanDecision::Abort).await.unwrap();

        assert_eq!(applied, Applied::Aborted);
        assert_eq!(
            incident.review_state,
            hive_common::protocol::IncidentReviewState::Aborted
        );
    }

    #[tokio::test]
    async fn a_note_is_typed_before_the_group_is_continued() {
        // A stopped process cannot read, but the tty buffers. Continuing first
        // would race the command finishing against the keys arriving.
        let control = FakeSession::live();
        let calls = control.calls.clone();
        let (desk, id) = desk(control);

        let (_, applied) = desk
            .decide(&id, &HumanDecision::ResumeWithNote("stay in /tmp".into()))
            .await
            .unwrap();

        assert_eq!(applied, Applied::ResumedWithNote);
        assert_eq!(calls.lock().unwrap().clone(), vec!["send:stay in /tmp", "resume"]);
    }

    #[tokio::test]
    async fn a_replacement_command_interrupts_the_old_one_first() {
        // "run instead", per HumanDecision::ModifyAndResume's own docs — so the
        // interrupt is queued, delivered by the continue, and only then is the
        // replacement typed.
        let control = FakeSession::live();
        let calls = control.calls.clone();
        let (desk, id) = desk(control);

        let (_, applied) = desk
            .decide(&id, &HumanDecision::ModifyAndResume("rm -rf ./build".into()))
            .await
            .unwrap();

        assert_eq!(applied, Applied::ModifiedAndResumed);
        assert_eq!(
            calls.lock().unwrap().clone(),
            vec!["interrupt", "resume", "send:rm -rf ./build"]
        );
    }

    #[tokio::test]
    async fn deciding_twice_is_refused_and_the_session_is_not_touched_again() {
        // The compare-and-swap is the whole reason recording precedes acting:
        // two operators on the review page must not resume and abort the same
        // suspended process.
        let control = FakeSession::live();
        let calls = control.calls.clone();
        let (desk, id) = desk(control);

        desk.decide(&id, &HumanDecision::Resume).await.unwrap();
        let second = desk.decide(&id, &HumanDecision::Abort).await;

        assert!(matches!(
            second,
            Err(DecisionError::Resolve(ResolveError::AlreadyResolved { .. }))
        ));
        assert_eq!(
            calls.lock().unwrap().clone(),
            vec!["resume"],
            "the losing decision must not reach the session"
        );
    }

    #[tokio::test]
    async fn a_session_that_already_ended_is_not_an_error() {
        // An agent finishing while a human deliberates is the common case. The
        // audit record still stands.
        let (desk, id) = desk(FakeSession::gone());
        let (incident, applied) = desk.decide(&id, &HumanDecision::Resume).await.unwrap();

        assert_eq!(applied, Applied::SessionAlreadyGone);
        assert!(incident.resolved_at.is_some());
    }

    #[tokio::test]
    async fn a_failed_action_says_the_decision_still_stands() {
        // The unclosable window: SQLite committed, the remote call did not.
        // Reporting it as a plain error would invite the operator to click
        // again, and the second click can only ever fail.
        let (desk, id) = desk(FakeSession::failing("resume"));
        let err = desk.decide(&id, &HumanDecision::Resume).await.unwrap_err();

        let msg = err.to_string();
        assert!(msg.contains("hive-w1"), "names the session: {msg}");
        assert!(msg.contains("cannot be re-submitted"), "says it is final: {msg}");

        // And it is genuinely final — the store agrees.
        let second = desk.decide(&id, &HumanDecision::Resume).await;
        assert!(matches!(
            second,
            Err(DecisionError::Resolve(ResolveError::AlreadyResolved { .. }))
        ));
    }

    #[tokio::test]
    async fn an_unknown_incident_never_touches_a_session() {
        let control = FakeSession::live();
        let calls = control.calls.clone();
        let (desk, _) = desk(control);

        let err = desk.decide("no-such-id", &HumanDecision::Abort).await;
        assert!(matches!(
            err,
            Err(DecisionError::Resolve(ResolveError::NotFound(_)))
        ));
        assert!(calls.lock().unwrap().is_empty());
    }
}
