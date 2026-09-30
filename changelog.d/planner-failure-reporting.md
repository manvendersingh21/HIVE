### Fixed

- A delegation plan that fails JSON parsing, the plan schema or validation is now
  reported as `Invalid plan: <class>: <error>` with the parser's line and column
  (for example "invalid JSON: expected `,` or `}` at line 1 column 6794" or "does not
  match the plan schema: missing field `device`") and HTTP 422, instead of
  "Planning timed out". This also applies when the planning deadline fires after a
  rejected plan.
- Every retry re-prompts the planner with the last parse or validation error,
  including the whole-plan retry after a deadline, instead of resending the same
  prompt.
- A model the planner picks that the device's inventory has not verified (for
  example agy `claude-sonnet-5` or cursor `gpt-5.6-sol`) falls back to the agent's
  default model, with a note in the plan summary, so the run no longer parks in
  needs-setup. A user-named unverified model is rejected back to the planner with
  the verified model list.
- Reviewer and verifier assignments own no paths, so they are no longer rejected for
  overlapping the implementer's owned paths.

### Added

- `[delegation] planning_deadline_secs` in `hive.toml` sets the delegation planning
  deadline (240 seconds by default).
