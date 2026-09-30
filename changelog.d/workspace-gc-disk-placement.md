### Fixed
- Collect terminal-run workspace build caches after a two-hour grace period through the coordinator's device transport, preserving live workspaces and recording reclaimed bytes.
- Reject Rust/frontend build placement on devices reporting less than 10 GiB free disk and highlight the requirement to the planner.
