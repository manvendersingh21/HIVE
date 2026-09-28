# Secure peer relay (WP-A)

The session page and `GET /api/runs/{id}/audit` identify this as
**Relay-attested (HACP Secure degraded mode)**. The coordinator attests the
message it received from a run's journal, not the integrity of that journal or
the agent's reasoning. A yolo agent shares its OS user with its runner; neither
is a security boundary. The coordinator database, its process and SSH routing
are the trust boundary. This is not end-to-end HACP encryption.

## Identity and signatures

Each created or replacement run has its own random Ed25519 identity. Existing
runs receive identities during the schema migration. Only `identity.public_key`
(hex Ed25519) and `identity.fingerprint` (SHA-256 of the public key bytes) appear
on a serialized run. Private seeds live exclusively in
`delegated_relay_keys` in the existing coordinator SQLite database. No new key
files are written and the database's file mode is unchanged. The seed is loaded
into zeroizing memory for signing and is never part of a run, assignment,
prompt, event, audit record or API response. Database backups remain sensitive.

The pinned HACP revision `7891da077d469e6308f31cbaa93d838c39cc54e6`
already enables the `guardian` feature. Its `hacp::secure::crypto::IdentitySecret`
(and seed import/export/sign/verify) is private. Its public guardian initializer
writes key files, and its public session manager cannot import/export identities
or sign relay envelopes. Those APIs cannot meet WP-A's database-only custody
requirement. `hive-core/src/delegation/relay.rs` therefore has a narrow custody
adapter using the same `ed25519-dalek`, `rand_core::OsRng`, and `zeroize`
primitives as HACP Secure, with strict Ed25519 verification. It reuses HACP v2
canonical JSON/digests directly. It does not fork HACP or expose arbitrary
signing. This is a deliberate API compatibility deviation from the brief's
literal request to generate the identity via `hacp::secure`; a future public
HACP database-custody interface can replace that small adapter.

At staging, an IMMEDIATE transaction allocates a strictly increasing sequence
for `(source, destination)`, builds the canonical envelope
`{id, task_id, source, destination, kind, text_digest, seq, staged_at}`, signs it,
and appends a `stage` audit event. `text_digest` is SHA-256 of the exact UTF-8
text; timestamps are UTC to seconds. The Ed25519 input is the UTF-8 prefix
`HIVE/HACP-SECURE/relay/v1\n` followed by HACP canonical JSON of the envelope.
Only `id`, `source`, `kind`, and `text` payload fields are permitted. The source
is assigned by the coordinator, never trusted from an agent's message.

Exact duplicate IDs do not allocate a second sequence or audit stage. Changed
IDs are rejected and audited as integrity incidents. Old unsigned queued
messages are rejected before delivery, not retroactively attested. User and
coordinator messages get separate coordinator-held identities and follow the
same verification path, but do not consume an agent's peer budget.

## Verification, ordering and budgets

Immediately before enqueueing, the coordinator recomputes the text digest,
checks routing and task membership against its stored message, verifies the
signature with the stored public identity, and requires the sequence to exceed
the pair's last delivered sequence. Corrupt/unsigned messages are quarantined,
never sent to the runner, and produce persistent `integrity` incidents visible
on both runs. Journal synchronization cannot overwrite this relay state.

Verification, budget reservation and a 300-second delivery lease share one
IMMEDIATE transaction. This prevents concurrent destination syncs from
overspending a task budget or overtaking an outstanding message for a pair.
Inbox acknowledgment advances the durable pair cursor and records `deliver`.
An SSH failure releases the lease; coordinator crashes expire it. A retry is
verified again, keeps its message ID (the runner inbox deduplicates it), and
consumes another budget slot. An old lease cannot acknowledge a newer attempt.
The normal transport timeout is shorter than the lease.

Configuration (positive integers, coordinator environment):

| Variable | Default | Meaning |
| --- | ---: | --- |
| `HIVE_RELAY_RUN_PER_MINUTE` | 10 | Peer enqueue attempts per source run |
| `HIVE_RELAY_TASK_PER_MINUTE` | 30 | Peer enqueue attempts across one task |

Limits use a rolling 60-second window, counted conservatively at the verified
delivery attempt. Exceeding either cap holds the message in SQLite and displays
the reason on the source and destination run. It is reconsidered on normal
synchronization and released when capacity returns. Held messages are not
removed. A held or leased message blocks later messages from that source to
the same destination; other source/destination pairs remain independent. Human
control messages remain available even when peer budgets are exhausted.

## Audit evidence

`delegated_relay_audit` is append-only (SQLite triggers reject UPDATE/DELETE).
Every row contains its position and the previous row's digest. Its digest is
HACP's SHA-256 canonical JSON digest of the entire record. A separate head
stores the final position/digest so missing tail rows, including deletion of
all rows, are detectable as well as missing middle rows and edits. The audit
records `stage`, `verify`, `deliver`, `hold` and `reject`; stage records include
the public identity, envelope and signature, never the message text or a seed.

The authenticated audit API verifies the entire global chain and returns
`chain_valid`, the head, and up to 100 recent entries involving the requested
run. `total_entries` is the run's full count. A broken chain is displayed as an
integrity failure, and a failed API request is never labeled verified. Run
list responses also carry independent `relay.incidents` and `relay.held` state.
This chain detects accidental corruption or partial tampering. An adversary
with full coordinator-database write access can replace both the chain and its
head; this is not an external notarization service.

## Verification

Rust regression coverage is in `hive-core/src/delegation/relay_tests.rs` and
`hive-web/src/delegation.rs`. Playwright covers public identity, incidents,
budget holds, verified/broken audit status and API unavailability on the
session page. See `docs/WP-A-VERIFICATION.md` for the implementation run's exact
CI commands and results.
