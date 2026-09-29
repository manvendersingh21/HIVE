### Fixed

- The delegation runner now pauses OpenCode quota errors from Z.ai (code 1308,
  "Usage limit reached for 5 hour. Your limit will reset at 2026-09-29
  22:01:59") instead of failing the run. The reset stamp carries no zone, so it
  is read in Z.ai's China time (UTC+8).
- Compact duration resets such as AGY's "Individual quota reached... Resets in
  2h0m42s" parse again: the unit pattern ended at a word boundary, which
  rejected digits directly after a unit letter. Runs 7593d039, 3adc4b7a and
  0dc4b500 went to `failed` through these gaps and now pause as `paused-quota`
  with the correct `resets_at`, never journaling a failed state.
- A quota pause releases the pending OpenCode message id, so the automatic
  resume turn after the reset is submitted instead of being rejected as an
  uncertain prior prompt.
