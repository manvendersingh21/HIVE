### Fixed

- Added recovery path for runs claimed into `launching` without a confirmed tmux session: included `launching` in `LIVE_STATES`, check session liveness after a bounded timeout marking dead runs as failed with a descriptive reason, and allowed `retry_setup` and `/reconcile` recovery.
- Prevented corrupted database rows from halting background sync and review loops: log every `RunStore::list()` failure with `warn!`, and skip unreadable rows with logged warnings rather than failing the entire list.
- Pruned expired `delegated_relay_budget` rows outside the 60-second rolling window on the message verification and budget check path.
