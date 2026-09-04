//! Regenerates the committed canonical schemas under `spec/schemas/` from the
//! `hacp::v2` wire types. Run after changing any wire type:
//!
//! ```text
//! cargo run -p hacp --bin emit-schemas
//! ```
//!
//! The gate test in `v2::schema` then keeps the committed files honest.

use hacp::v2::schema::{schema_dir, wire_schemas};

fn main() {
    let dir = schema_dir();
    std::fs::create_dir_all(&dir).expect("create the schemas directory");
    for (name, canonical) in wire_schemas() {
        let path = dir.join(format!("{name}.json"));
        std::fs::write(&path, canonical + "\n")
            .unwrap_or_else(|e| panic!("writing {}: {e}", path.display()));
        println!("wrote {}", path.display());
    }
}
