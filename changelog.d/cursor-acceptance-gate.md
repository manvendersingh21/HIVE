### Added

- `scripts/check-cursor-agent.py`, a mechanical acceptance gate for the Cursor
  delegated agent. Each acceptance criterion is one named check, evaluated
  offline with no network, model call or worker CLI, and the script exits
  non-zero on the first failure. Seven checks cover the runner registration
  (`cursor` resolves to the `cursor-agent` binary), the probe's
  version/readiness/authentication/model reporting, the eight cursor adapter
  contract tests, the planner's three accept sites, the container worker
  probes, the frontend event mapping and the nine pinned cursor rendering
  tests. `--frontend` additionally runs the `runEvents` spec through Playwright.
- The gate runs in CI, and the Cursor planner test is now invoked by name, so
  the feature is measured on every push instead of asserted in a report.

### Fixed

- The delegation runner contract suite (`test_runner` and `test_adapters`,
  85 tests) was never executed by CI, so adapter regressions could merge
  green. It now runs on both the Ubuntu and macOS matrix legs.
