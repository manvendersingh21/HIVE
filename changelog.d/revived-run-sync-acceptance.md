### Added

- `scripts/check-delivery-acknowledgment.py` and a `delivery-acknowledgment` CI
  job pin the acceptance contract for revived completed runs: a run that has been
  handed a message keeps syncing until its journal acknowledges that exact
  message ID and reports the state the turn ended in. The check is static, so it
  runs on every push, and CI also runs the Rust test
  `a_revived_completed_run_is_synced_until_its_message_is_acknowledged` that
  exercises the behaviour. Either half failing fails the build, so the tracking,
  the `idle` predicate consulting it, and the test that proves it can no longer
  be dropped independently.
