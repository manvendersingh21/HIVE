### Fixed

- The chat page no longer crashes on an approval reply without an `awaiting_approval` list, and approval steps missing a target or command render with neutral fallbacks.
- The session page no longer crashes on relay audit entries without a `detail`, or on teammates without `owned_paths` or `dependencies` (shown as "None assigned" and "None").
- The session page stops polling the run, team roster and relay audit once every run in the task has reached a terminal state, and no longer fetches the run list twice when it opens.
- Settings → Containers keeps the machine fixed while containers are being listed, and drops a list that comes back for a different machine.
- The Sessions page shows a non-JSON `x-hive-session-errors` header as a plain-text warning instead of failing to load machines.
