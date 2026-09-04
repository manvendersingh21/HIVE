//! The run state machine (`spec/HACP.md` §7, §9, §11.1).
//!
//! The transition table is data rather than scattered `if`s so that an implementation
//! can be checked against it, and so the conformance vectors can assert on it directly.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Where a run is.
///
/// ```text
/// formation -> planning -> drafted <-> amending -> frozen -> working -> reporting
///                                         ^                                |
///                                         +-- amended (post-freeze, §9.2) --+
///                                                                          v
///                              verifying -> reworking -> verifying -> integrating -> completed
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RunState {
    /// Sizing the team and decomposing the goal (§7). New in 1.1.
    Formation,
    Planning,
    /// A draft is broadcast and responses are open.
    Drafted,
    /// An amendment or change request is being adjudicated. Reachable both before
    /// freeze (§9.1) and after it (§9.2).
    Amending,
    /// Frozen with interface digests; work may start.
    Frozen,
    Working,
    Reporting,
    Verifying,
    /// A failed verdict is being repaired (§11.1). New in 1.1.
    Reworking,
    Integrating,
    Completed,
    Failed,
    Aborted,
    TimedOut,
}

impl RunState {
    /// Whether this state can still transition.
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Aborted | Self::TimedOut)
    }

    /// Whether the contract is frozen in this state — i.e. whether a change to a frozen
    /// interface file now requires a change request (§9.2).
    pub fn is_post_freeze(&self) -> bool {
        matches!(
            self,
            Self::Frozen
                | Self::Working
                | Self::Reporting
                | Self::Verifying
                | Self::Reworking
                | Self::Integrating
        )
    }

    /// The states reachable from here, excluding the terminal states that are reachable
    /// from everywhere (see [`RunState::can_transition_to`]).
    pub fn successors(&self) -> &'static [RunState] {
        use RunState::*;
        match self {
            Formation => &[Planning],
            Planning => &[Drafted, Frozen],
            Drafted => &[Amending, Drafted, Frozen],
            // Amending returns to Drafted before freeze and to Frozen after it (§9.2),
            // which is why both are legal successors of the one state.
            Amending => &[Drafted, Frozen],
            Frozen => &[Working, Amending],
            Working => &[Reporting, Amending],
            Reporting => &[Verifying],
            Verifying => &[Reworking, Integrating],
            Reworking => &[Verifying],
            Integrating => &[Completed],
            Completed | Failed | Aborted | TimedOut => &[],
        }
    }

    /// Whether `self -> next` is legal.
    ///
    /// Every terminal state except `Completed` is reachable from any non-terminal state:
    /// a run must always be able to fail, be aborted, or time out. `Completed` is
    /// reachable only through integration, because "completed" is a claim about
    /// verification having happened.
    pub fn can_transition_to(&self, next: RunState) -> bool {
        if self.is_terminal() {
            return false;
        }
        if matches!(next, RunState::Failed | RunState::Aborted | RunState::TimedOut) {
            return true;
        }
        self.successors().contains(&next)
    }

    pub fn label(&self) -> &'static str {
        match self {
            Self::Formation => "formation",
            Self::Planning => "planning",
            Self::Drafted => "drafted",
            Self::Amending => "amending",
            Self::Frozen => "frozen",
            Self::Working => "working",
            Self::Reporting => "reporting",
            Self::Verifying => "verifying",
            Self::Reworking => "reworking",
            Self::Integrating => "integrating",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Aborted => "aborted",
            Self::TimedOut => "timed out",
        }
    }

    /// Every state, for exhaustive checks.
    pub const ALL: &'static [RunState] = &[
        Self::Formation, Self::Planning, Self::Drafted, Self::Amending, Self::Frozen,
        Self::Working, Self::Reporting, Self::Verifying, Self::Reworking,
        Self::Integrating, Self::Completed, Self::Failed, Self::Aborted, Self::TimedOut,
    ];
}

impl std::fmt::Display for RunState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.label())
    }
}

/// Body of `run.started`.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct RunStartedBody {
    pub goal: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deadline_secs: Option<u64>,
    /// All worker URNs on the run — identities only, never contact details (§3).
    pub participants: Vec<String>,
}

/// Body of `run.failed`.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct RunFailedBody {
    pub reason: String,
    /// The state the run was in when it failed.
    pub last_state: RunState,
    /// Paths, relative to the run root, kept for inspection.
    #[serde(default)]
    pub preserved_paths: Vec<String>,
}

/// The bounds that guarantee a run terminates (§9.1, §9.2, §11.1, §12).
///
/// Every one of these exists because some loop would otherwise be unbounded. They are
/// gathered in one struct so an implementation cannot quietly omit one.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema)]
pub struct RunLimits {
    /// Negotiation rounds before freeze.
    pub max_rounds: u32,
    /// Amendments accepted after freeze.
    pub max_amendments: u32,
    /// Rework rounds after a failed verdict. Zero is conformant and means "fail honestly
    /// on first failure".
    pub max_rework_rounds: u32,
    /// How often a worker should report liveness.
    pub heartbeat_secs: u64,
}

impl Default for RunLimits {
    fn default() -> Self {
        Self { max_rounds: 2, max_amendments: 3, max_rework_rounds: 1, heartbeat_secs: 30 }
    }
}
