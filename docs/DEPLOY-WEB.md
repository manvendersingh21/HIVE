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
