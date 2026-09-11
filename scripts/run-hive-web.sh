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

exec "$HOME/hive/target/release/hive-web"
