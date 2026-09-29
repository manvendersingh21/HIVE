### Fixed

- Hardened `RunStore::list()` against database corruption: scanning continues past unreadable rows and corrupt SQLite pages, skipping corrupted rows while surfacing an error count and returning all readable runs.
- Updated `RunStore::create()` to return exactly the inserted run IDs without re-listing the entire table, and ensured plans containing assignments that yield zero runs fail loudly instead of reporting completed with `runs: []`.
- Added startup database integrity check with `PRAGMA quick_check` and exposed database status (`db_ok`) in `/api/health`.
- Audited test suites and maintenance scripts to guarantee proper temp `HOME` environment configuration, eliminate concurrent database connections on individual files, and cleanly remove `-wal` and `-shm` sidecar files after closing connections.
