#!/bin/bash
# Phase 3 exit test: the reference HACP/2.0 implementation against the
# independent Python peer (spec + schemas + goldens only) over the file edge.
set -euo pipefail
cd "$(dirname "$0")/.."
if [ -z "${HACP_CHECKOUT:-}" ] || [ ! -f "$HACP_CHECKOUT/Cargo.toml" ]; then
    echo "HACP now lives at https://github.com/manvendersingh21/hcap" >&2
    echo "Set HACP_CHECKOUT to a protocol checkout, then rerun this script." >&2
    exit 2
fi
exec cargo test --manifest-path "$HACP_CHECKOUT/Cargo.toml" --locked \
    --test v2_interop -- --nocapture "$@"
