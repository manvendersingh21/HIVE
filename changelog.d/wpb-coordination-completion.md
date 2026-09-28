### Added
- Session pages show bilateral HACP v2 frozen contract digests, pending amendments and coordinator acceptance evidence on both participants' sessions.
- Delegation plans include mechanical file and command checks and a configurable acceptance rework limit (default two). The coordinator runs checks through the runner, reuses the runtime attestation gate, persists evidence and sends repair feedback to the same conversation before terminal `no_agreement`.

### Fixed
- Ownership validation catches wildcard prefixes inside filenames and validates single-assignment paths.
- Native turn completion and model review cannot complete delegated work without coordinator-measured acceptance. Quota pauses and peer waits do not consume rework; replayed or stale measurements cannot spend a round twice.
