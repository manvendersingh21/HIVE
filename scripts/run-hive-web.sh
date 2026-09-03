#!/bin/bash
# launchd wrapper for hive-web on the master.
#
# The password lives in ~/.config/hive/web.env (mode 600) rather than in the
# plist, which is world-readable.
set -a
. "$HOME/.config/hive/web.env"
set +a

# Bind the Tailscale address, never 0.0.0.0 — this machine is on untrusted
# networks and the agent can run shell commands.
export HIVE_WEB_ADDR="${HIVE_WEB_ADDR:-100.121.248.111:8090}"
export HIVE_CONFIG_ROOT="$HOME/hive"
export HIVE_WEB_STATIC="$HOME/hive/hive-web/static"
export HIVE_MASTER_NAME="manus-mac-mini"
export RUST_LOG="${RUST_LOG:-hive_web=info,hive_core=info}"

# Ollama and the worker CLIs live outside launchd's minimal PATH.
export PATH="/opt/homebrew/bin:/usr/local/bin:$HOME/.local/bin:$PATH"

exec "$HOME/hive/target/release/hive-web"
