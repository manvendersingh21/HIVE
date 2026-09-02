//! Safety watchdog — continuous monitoring of agent sessions with human-in-the-loop.

/// Safety watchdog configuration and state.
pub struct Watchdog {
    // TODO: ractor Actor, monitored sessions, rules, notifier
}

impl Watchdog {
    /// Create a new watchdog (not started).
    pub fn new() -> Self {
        Self {}
    }
}
