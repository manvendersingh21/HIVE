# WP-A implementation verification

Device: mac-air. Workspace: `/Users/manubaba/hive-workspaces/wp-a-implement`.
Branch: `feat/guardrails-secure-relay`, based on `origin/main` at `0218634`.
Date: 2026-09-28. PR target: `main`; no merge.

## Required CI commands

Executed the steps in `.github/workflows/ci.yml` locally. Toolchains:
Rust stable 1.98.1, Python 3.11.15, Node 22.23.3 and npm 10.9.9.

```sh
rustup toolchain install stable --profile minimal
rustup default stable
python3 --version
export CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=4 CARGO_TARGET_DIR="$PWD/target"
export RUSTFLAGS='-D warnings'
cargo build --workspace --locked
cargo test --workspace --locked --no-run
cargo test --workspace --locked
python3 scripts/check-nvidia.py
python3 scripts/check-chat-history.py
git diff --check

cd hive-web/frontend
npm ci
npx tsc --noEmit
npm run build
npx playwright install --with-deps chromium
CI=true npx playwright test
```

To match the workflow's Node 22 rather than the machine's default Node, the
frontend sequence was also executed with this exact toolchain wrapper:

```sh
npm exec --yes --package=node@22 --package=npm@10 -- sh -c 'node --version && npm --version && npm ci && npx tsc --noEmit && npm run build && npx playwright install --with-deps chromium && CI=true npx playwright test'
```

## Results and evidence

- Workspace build and test compilation: pass with warnings denied.
- Workspace tests: 544 passed, 0 failed; 18 pre-existing ignored tests and one
  ignored documentation example. All 10 new relay store regression tests pass.
- NVIDIA integration: classification, planning, passage/query embeddings,
  harmless CLI task and memory indexing/reindex/query, authenticated web chat:
  pass; 14 mock requests checked.
- Chat-history integration: persistence, isolation, idempotency, authentication,
  restart, failure feedback, dependency stop, false-completion rejection,
  correction/verification, approval recovery and browser disconnect: pass.
- Frontend dependency install: pass, no reported vulnerabilities. TypeScript
  and production static build: pass.
- Playwright on pinned Chromium and Node 22: 127 passed, 4 existing opt-in live
  tests skipped (real backend/agent environment required), 0 failed. Both new
  relay UI tests passed.
- `git diff --check`: pass.
- Visually inspected `/tmp/wp-a-relay-session.png`: the session panel's degraded
  mode label, public key/fingerprint, verified audit status and event details
  fit the sidebar; integrity incidents and budget holds appear in the main pane.

Local logs (kept outside the build directory so target cleanup preserves them):
`/tmp/wp-a-cargo-build.log`, `/tmp/wp-a-cargo-no-run.log`,
`/tmp/wp-a-cargo-tests.log`, `/tmp/wp-a-nvidia.log`,
`/tmp/wp-a-chat-history.log`, `/tmp/wp-a-frontend-node22.log`.

## Security regression coverage

- Distinct identities for created/replacement runs, stable identities on
  reopening, no seed in serialized runs, assignments, debug output or audit.
- Modified text, kind, source, extra fields, malformed JSON, signature,
  envelope/task/sequence, missing attestation: no inbox delivery; persistent
  integrity incident on both runs.
- Exact ID replay is idempotent; changed ID payloads are rejected/audited;
  delivered or out-of-order sequences are rejected independently of ID checks.
- Per-run and per-task rolling budgets, exact 60-second release, cross-task
  independence, human-control availability, durable holds, concurrent claims.
- Pair ordering while a lease is outstanding, crash lease expiration, stale
  acknowledgments refused, failed transport retry verified again.
- Audit update/delete triggers; independent corruption simulation detects
  changed, removed middle, removed tail and entirely removed audit rows.
- Existing unsigned messages fail closed; runner snapshots cannot erase relay
  incidents. Audit API returns only public evidence and requires authentication.
- UI displays holds/incidents, public identity, valid/broken chain status, and
  unavailability without falsely claiming verification.

## Review note

The upstream pinned `hacp::secure` has no public database-custody or relay
signing API. The implementation reuses HACP canonical JSON/digests and the
same Ed25519/OS-entropy/zeroizing backend in a small coordinator-only adapter,
rather than writing private keys to guardian files. This deviation from the
brief's literal `hacp::secure` key-generation instruction is explicit in
[HACP-RELAY.md](HACP-RELAY.md). No end-to-end or agent-reasoning integrity claim
is made. Independent verifier acceptance is required before opening the PR.
