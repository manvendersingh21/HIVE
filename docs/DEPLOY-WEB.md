# Deploying `hive-web`

Live deployment: the `lawfinder` Azure VM (Ubuntu 24.04), reachable on the tailnet
at **http://100.93.65.98:8090**.

## Security model

The network is the primary boundary, and the password is the second layer.

1. **`hive-web` binds the Tailscale interface address only** (`100.93.65.98:8090`),
   never `0.0.0.0`. This box has a public Azure IP (`20.83.33.159`); binding
   `0.0.0.0` would publish a root-capable shell to the open internet. Verified:
   the public IP does not answer on 8090.
2. **Tailscale (WireGuard) encrypts the transport**, so plain HTTP here is still
   end-to-end encrypted between tailnet devices — it just has no browser padlock.
3. **A password gates every route** except `/login` and `/api/health`. Tokens are
   in-memory, so a restart logs everyone out. Set via `HIVE_WEB_PASSWORD`;
   the server refuses to start without it, or with fewer than 8 characters.

The server intentionally runs unsandboxed: its job is to spawn interactive shells
as the user, so systemd hardening directives would defeat the feature.

### Upgrading to HTTPS

Serve/HTTPS is **not enabled on this tailnet**, so `tailscale serve` fails with
"Serve is not enabled" and `tailscale cert` returns "your Tailscale account does
not support getting TLS certs". Enable it once in the admin console, then:

```bash
sudo tailscale serve --bg --https=443 http://127.0.0.1:8090
sed -i 's|^HIVE_WEB_ADDR=.*|HIVE_WEB_ADDR=127.0.0.1:8090|' ~/.config/hive/web.env
sudo systemctl restart hive-web
```

That moves the bind back to loopback and puts a real Let's Encrypt cert in front
at `https://lawfinder.tailc13673.ts.net`. Do **not** swap `serve` for `funnel`
without deciding to expose a shell to the public internet.

## Layout

| Path | Purpose |
|:---|:---|
| `/home/azureuser/hive` | source checkout |
| `/home/azureuser/hive/target/release/hive-web` | binary |
| `~/.config/hive/web.env` | `HIVE_WEB_PASSWORD`, bind addr, static dir (mode 600) |
| `/etc/systemd/system/hive-web.service` | unit, `enabled` so it survives reboot |

## Redeploy

```bash
rsync -az --delete --exclude target/ --exclude .git/ \
  -e "ssh -i ~/Downloads/lawfinder_key.pem" ./ azureuser@100.93.65.98:~/hive/
ssh -i ~/Downloads/lawfinder_key.pem azureuser@100.93.65.98 \
  'cd ~/hive && . ~/.cargo/env && cargo build --release -p hive-web && sudo systemctl restart hive-web'
```

Logs: `journalctl -u hive-web -f`

## `~/.local/bin` on the non-interactive PATH

`claude` and `codex` are symlinks in `~/.local/bin`, which `~/.profile` only adds
for **login** shells. Hive's SSH delegation runs commands non-interactively, where
`~/.bashrc` returns early on its "If not running interactively, don't do anything"
guard — so `ssh host claude ...` failed with *command not found*.

Fixed by prepending a `PATH` export **above** that guard in `~/.bashrc`
(marked `HIVE_PATH_FIX`). `ssh host claude --version` now resolves.

For the same reason, sessions launch through `bash -lc`, not a bare exec.
Two consequences worth knowing:

- `tmux`'s `pane_current_command` reports `bash`, since the tool is a child of the
  login shell. The dashboard shows the tmux **window name** instead, set to the
  session kind at creation.
- `codex` refuses to start outside a git repo, so it is launched with
  `--skip-git-repo-check`.

---

## The master instance (agent UI)

The same binary runs on the Mac Mini, where it additionally serves the agent
chat and the machine graph. It decides which of the two modes it is in by
**probing for a local model**, not by reading config: a host with no reachable
Ollama serves terminals only and says so through `/api/capabilities`, so the UI
hides the chat rather than offering one that fails on the first message.

| | master (`manus-mac-mini`) | worker (`lawfinder`) |
|:---|:---|:---|
| URL | `http://100.121.248.111:8090` | `http://100.93.65.98:8090` |
| Chat | yes | no — `/api/chat` returns 503 |
| Terminal | yes | yes |
| Supervisor | launchd `dev.hive.web` | systemd `hive-web` |

Master service files:

| Path | Purpose |
|:---|:---|
| `~/Library/LaunchAgents/dev.hive.web.plist` | launchd job, `RunAtLoad` + `KeepAlive` |
| `scripts/run-hive-web.sh` | wrapper: loads the env file, sets bind address and PATH |
| `~/.config/hive/web.env` | `HIVE_WEB_PASSWORD` (mode 600 — the plist is world-readable) |
| `~/.hive/hive.db` | SQLite knowledge graph |
| `~/.hive/web.log` | combined stdout/stderr |

```bash
launchctl unload ~/Library/LaunchAgents/dev.hive.web.plist
launchctl load   ~/Library/LaunchAgents/dev.hive.web.plist
tail -f ~/.hive/web.log
```

The wrapper sets `PATH` explicitly because launchd starts jobs with a minimal
one that excludes Homebrew — without it, `ollama` and the agentic CLIs are
invisible to the agent even though they work in your shell.

## The local approval gate

`POST /api/chat` plans, runs everything the watchdog is happy with, and stops at
anything its Tier-1 rules flag. Those steps come back as `awaiting_approval` and
are resumed through `POST /api/chat/{run_id}/approve`.

The plan is held **server-side** between the two calls. The browser sends step
ids, never command text, so an approval cannot be turned into a different
command than the one the user was shown. A completed run is dropped from the
pending map, so replaying an approval returns 404 rather than re-running it.

Only local execution is gated this way. Delegated remote steps are supervised
live by the same watchdog once their tmux session starts, which is a stronger
guarantee than a pre-flight check — it keeps watching after the command begins.

---

## The worker daemon (`hive-worker`)

Deployed on `lawfinder` as systemd `hive-worker`, listening on the Tailscale
address `100.93.65.98:9091`.

| Path | Purpose |
|:---|:---|
| `/etc/systemd/system/hive-worker.service` | unit, enabled |
| `~/.config/hive/worker.env` | bind addr, worker name, master URL, token (mode 600) |
| `/tmp/hive-task-<task_id>.log` | per-task combined output |

### Authentication is mandatory

`POST /task` executes arbitrary shell as `azureuser`. The daemon **refuses to
start** without `HIVE_WORKER_TOKEN` (minimum 16 characters); every endpoint
except `/health` requires `Authorization: Bearer <token>`. `/health` stays open
so reachability probes need no credential.

`HIVE_WORKER_ALLOW_UNAUTHENTICATED=1` overrides the refusal. Use it only on a
loopback bind — on a routable address it is a remote code execution endpoint.

The master's `HIVE_WORKER_TOKEN` must match: it is what `hive-worker` sends on
status callbacks, and what `POST /api/worker/status` checks.

### Endpoints

| Method | Path | Purpose |
|:---|:---|:---|
| GET | `/health` | liveness, worker name, whether callbacks are configured |
| POST | `/task` | accept a `TaskAssignment`; returns `202` immediately |
| GET | `/tasks` | every task this daemon has accepted, newest first |
| GET | `/status/{id}` | real state, exit code, and output; `404` if unknown |
| POST | `/task/{id}/pause` | SIGSTOP the task's process group |
| POST | `/task/{id}/resume` | SIGCONT it |
| POST | `/task/{id}/kill` | end the session; state becomes `cancelled` |

### Signalling safety

Pause targets the tty's **foreground process group**, not the pane's shell —
commands arrive via `send-keys` and run as a job in their own group, so
signalling the shell would freeze the wrong thing. The pgid is recorded at pause
time because bash reclaims the terminal once a job stops.

Signals go through `libc::killpg`, never a `kill` subprocess: a negative argument
to `kill` is overloaded, and `-1` means *every process the user can signal*. See
the incident note in [`ROADMAP.md`](ROADMAP.md#phase-4--worker-daemon) — that
mistake stopped a host's unrelated services during testing. `validate_pgid`
refuses `<= 1` and the daemon's own group, and `pause` checks the pgid is on the
target session's tty.
