//! Canonical JSON Schema emission (`spec/HACP-2.0-draft.md` §14).
//!
//! Schemas are first-class deliverables, derived from the semantic model — never the
//! reverse. Every HACP/2.0 wire type that lands in this crate registers itself in
//! [`wire_schemas`], whose output is the canonical JSON (§5.1 rules) of its JSON
//! Schema. The committed copies under `spec/schemas/` are the contract an
//! independent implementer builds against; the gate test below fails on drift
//! between the types and those files, in either direction, including stale files.

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde_json::Value;

use super::canon::{canonical_json, CanonError};

/// Render a type's JSON Schema in canonical form: sorted keys, no whitespace,
/// integers only. Two builds of the same type produce byte-identical schema files,
/// which is what makes the committed copies reviewable as diffs.
///
/// Schema emission applies one boundary-specific normalization first: schemars
/// emits numeric bounds as whole-valued floats (`"minimum": 0.0` for a `u64`),
/// which §5.1's integers-only rule exists to forbid in protocol objects. Whole
/// floats within u64/i64 range become their integer form — semantically identical
/// to any JSON Schema validator, digest-stable here. A non-whole float is left
/// alone and fails canonicalization loudly rather than being silently rounded.
pub fn canonical_schema_json<T: JsonSchema>() -> Result<String, CanonError> {
    let schema = schemars::schema_for!(T);
    let mut value = serde_json::to_value(&schema).expect("a schema always serializes");
    normalize_schema_numbers(&mut value);
    canonical_json(&value)
}

fn normalize_schema_numbers(value: &mut Value) {
    match value {
        Value::Number(n) => {
            // Already an integer representation? Nothing to do.
            if n.as_u64().is_some() || n.as_i64().is_some() {
                return;
            }
            if let Some(f) = n.as_f64() {
                if f.is_finite() && f.fract() == 0.0 {
                    if f >= 0.0 {
                        *n = serde_json::Number::from(f as u64);
                    } else {
                        *n = serde_json::Number::from(f as i64);
                    }
                }
            }
        }
        Value::Array(items) => {
            for item in items {
                normalize_schema_numbers(item);
            }
        }
        Value::Object(map) => {
            for (_, item) in map.iter_mut() {
                normalize_schema_numbers(item);
            }
        }
        _ => {}
    }
}

/// The registry of wire schemas, name → canonical schema JSON.
///
/// Populated as v2 wire types land (§① envelope in the kernel phase, then sessions,
/// contracts, artifacts). An entry appearing here without a matching committed file
/// under `spec/schemas/` — or a committed file without an entry — fails the gate.
pub fn wire_schemas() -> BTreeMap<&'static str, String> {
    into_registry([
        ("envelope", canonical_schema_json::<super::Envelope>()),
        ("agent", canonical_schema_json::<super::Agent>()),
        ("session", canonical_schema_json::<super::Session>()),
        ("contract", canonical_schema_json::<super::Contract>()),
        ("artifact", canonical_schema_json::<super::Artifact>()),
        ("evidence", canonical_schema_json::<super::Evidence>()),
        ("verification", canonical_schema_json::<super::Verification>()),
    ])
}

fn into_registry(
    entries: impl IntoIterator<Item = (&'static str, Result<String, CanonError>)>,
) -> BTreeMap<&'static str, String> {
    let mut map = BTreeMap::new();
    for (name, result) in entries {
        let canonical = result.unwrap_or_else(|e| {
            panic!("wire schema {name} did not canonicalize — a type broke §5.1: {e}")
        });
        map.insert(name, canonical);
    }
    map
}

/// Where committed schemas live, resolved from the crate root so tests behave the
/// same regardless of the working directory.
pub fn schema_dir() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("spec/schemas")
}

#[cfg(test)]
mod tests {
    use super::*;

    // Undocumented on purpose: doc comments become schema descriptions, and the
    // canonical-form assertions below check for structural whitespace, which must
    // come from the serialization, never from string contents.
    #[derive(JsonSchema)]
    #[allow(dead_code)]
    struct Probe {
        name: String,
        count: u64,
    }

    #[test]
    fn schema_rendering_is_canonical_and_stable() {
        let a = canonical_schema_json::<Probe>().unwrap();
        let b = canonical_schema_json::<Probe>().unwrap();
        assert_eq!(a, b, "two renders of one type must be byte-identical");
        // Canonical form properties, on a real schema: sorted keys, single line.
        assert!(a.contains("\"properties\":"), "got: {a}");
        assert!(!a.contains('\n'), "line break in: {a}");
        // schemars' whole-float bounds arrived as integers (0.0 became 0).
        assert!(a.contains("\"minimum\":0"), "got: {a}");
        assert!(!a.contains("0.0"), "float survived: {a}");
        let reparsed: serde_json::Value = serde_json::from_str(&a).unwrap();
        assert_eq!(canonical_json(&reparsed).unwrap(), a, "not idempotent");
    }

    #[test]
    fn non_whole_floats_still_fail_rather_than_round() {
        // The normalization is for schemars' whole bounds, not a general mercy.
        let mut value = serde_json::json!({"confidence": 0.5});
        normalize_schema_numbers(&mut value);
        assert!(
            canonical_json(&value).is_err(),
            "a real float must fail canonicalization"
        );
    }

    #[test]
    fn the_registry_and_the_committed_directory_agree() {
        let schemas = wire_schemas();
        let dir = schema_dir();
        std::fs::create_dir_all(&dir).expect("schemas dir exists");
        let mut committed: Vec<String> = std::fs::read_dir(&dir)
            .expect("schemas dir is readable")
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().map(|t| t.is_file()).unwrap_or(false))
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".json"))
            .collect();
        committed.sort();
        let mut registered: Vec<String> =
            schemas.keys().map(|n| format!("{n}.json")).collect();
        registered.sort();
        assert_eq!(
            committed, registered,
            "spec/schemas and wire_schemas() must list the same files; \
             regenerate or remove the stray"
        );
        for (name, canonical) in &schemas {
            let path = dir.join(format!("{name}.json"));
            let on_disk = std::fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("committed schema {name} unreadable: {e}"));
            assert_eq!(
                on_disk.trim_end(),
                canonical,
                "committed schema {name} drifted from the type; regenerate it"
            );
        }
    }
}
