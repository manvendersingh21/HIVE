# Canonical JSON Schemas

One file per HACP/2.0 wire type, rendered in canonical form (`spec/HACP-2.0-draft.md`
§5.1) and derived from the `hacp::v2` types via `wire_schemas()`. The schema-diff gate
in `hacp/src/v2/schema.rs` fails the build on drift in either direction, including
stale files. Independent implementers build against these files, the draft spec, and
the golden transcripts — never against the Rust source.
