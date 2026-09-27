# Hive web frontend

This is the TypeScript React/Next.js frontend for `hive-web`. It uses a static
export so the Rust service can serve it directly while retaining the existing
API and WebSocket endpoints.

```bash
npm ci
npm run build
rsync -a --delete out/ ../static/
```

`../static/` is build output and is not committed. `scripts/run-hive-web.sh`
rebuilds it when this source changes, so restarting the service is enough.

The pages are implemented in `app/`: agent chat, sessions, machines,
incidents, login, and the xterm-backed terminal.
