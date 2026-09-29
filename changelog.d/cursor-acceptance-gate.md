### Added

- `scripts/check-cursor-agent.py`, a mechanical acceptance gate for the Cursor
  delegated agent. Each acceptance criterion is one named check, evaluated
  offline with no network, model call or worker CLI, and the script exits
  non-zero on the first failure. Seven checks cover the runner registration
  (`cursor` resolves to the `cursor-agent` binary), the probe's
  version/readiness/authentication/model reporting, the eight cursor adapter
  contract tests including a new one that proves the probe rejects a CLI that is
  missing any one documented flag, the planner's three accept sites, the container worker
  probes, the frontend event mapping and the nine pinned cursor rendering
  tests. `--frontend` additionally runs the `runEvents` spec through Playwright.
- The gate runs in CI, and the Cursor planner test and the cursor event
  rendering spec are now invoked by name with a guard on their own result
  lines, so the feature is measured on every push instead of asserted in a
  report. Both guards exist because a name filter exits zero when it matches
  nothing, and a declared-but-skipped test still counts as declared.

### Fixed

- The delegation runner contract suite (`test_runner` and `test_adapters`,
  85 tests) was never executed by CI, so adapter regressions could merge
  green. It now runs on both the Ubuntu and macOS matrix legs.
