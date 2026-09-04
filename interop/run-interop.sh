#!/bin/bash
# Phase 3 exit test: the reference HACP/2.0 implementation against the
# independent Python peer (spec + schemas + goldens only) over the file edge.
set -euo pipefail
cd "$(dirname "$0")/.."
exec cargo test -p hacp --test v2_interop -- --nocapture "$@"
