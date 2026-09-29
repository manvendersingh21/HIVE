### Fixed

- Coordinator review claims now issue and store a unique claim token; `finish_review` only commits and increments `continue_reviews` when called by the current holder of that token, preventing expired claims from erroneously incrementing the round counter.
- `scripts/changelog.py release <version>` refuses a version that already exists in `CHANGELOG.md` before writing anything or deleting fragments.
- `scripts/test_changelog.py` can be executed directly as a script via `python3 scripts/test_changelog.py`.
