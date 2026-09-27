#!/bin/bash
# launchd wrapper for hive-web on the master.
#
# This checkout uses local Ollama for reasoning and embeddings; keep Ollama running.
# The password lives in ~/.config/hive/web.env (mode 600) rather than in the
# plist, which is world-readable.
set -a
. "$HOME/.config/hive/web.env"
set +a

# Bind address should be configured externally. Default to localhost for safety.
# Set HIVE_WEB_ADDR in ~/.config/hive/web.env to override (e.g. "127.0.0.1:8090").
export HIVE_WEB_ADDR="${HIVE_WEB_ADDR:-127.0.0.1:8090}"

# Configuration paths - customize as needed
export HIVE_CONFIG_ROOT="${HIVE_CONFIG_ROOT:-$HOME/hive}"
export HIVE_WEB_STATIC="${HIVE_WEB_STATIC:-$HOME/hive/hive-web/static}"
export HIVE_MASTER_NAME="${HIVE_MASTER_NAME:-$(hostname)}"

# Log level configuration
export RUST_LOG="${RUST_LOG:-hive_web=info,hive_core=info}"

# Ensure required paths are available
export PATH="/opt/homebrew/bin:/usr/local/bin:$HOME/.local/bin:$PATH"

# The frontend export isn't committed. Rebuild it into the default static dir
# whenever the frontend source is newer than the last build; a custom
# HIVE_WEB_STATIC is served as given. A failed rebuild keeps serving the
# previous build rather than taking the site down.
FRONTEND="$HOME/hive/hive-web/frontend"
STAMP="$HIVE_WEB_STATIC/.built"
if [ "$HIVE_WEB_STATIC" = "$HOME/hive/hive-web/static" ] && {
    [ ! -f "$STAMP" ] ||
    [ -n "$(find "$FRONTEND/app" "$FRONTEND/components" "$FRONTEND/lib" \
        "$FRONTEND/package.json" "$FRONTEND/package-lock.json" "$FRONTEND"/next.config.* \
        -newer "$STAMP" -print -quit 2>/dev/null)" ]
}; then
    echo "run-hive-web: building frontend" >&2
    if (cd "$FRONTEND" &&
        { [ "node_modules/.package-lock.json" -nt package-lock.json ] || npm ci --no-audit --no-fund; } &&
        npm run build) >&2 &&
        mkdir -p "$HIVE_WEB_STATIC" &&
        rsync -a --delete "$FRONTEND/out/" "$HIVE_WEB_STATIC/"; then
        touch "$STAMP"
    elif [ -f "$HIVE_WEB_STATIC/index.html" ]; then
        echo "run-hive-web: frontend build failed; serving the previous build" >&2
    else
        echo "run-hive-web: frontend build failed and no previous build exists" >&2
        exit 1
    fi
fi

exec "$HOME/hive/target/release/hive-web"
