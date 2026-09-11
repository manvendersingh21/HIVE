# ws-share

Send and download files between the MacBook Air and Arch Linux through authenticated
WebSockets on their Tailscale addresses. Each server shares only `~/HiveShare`.

On either computer:

```sh
ws-share status
ws-share send "/path/to/file.pdf"
ws-share send                          # file picker on macOS; Linux picker or path prompt
ws-share list                         # list files on the other computer
ws-share get "file.pdf" ~/Downloads/   # retrieve a file from the other computer
```

The other computer is the default destination. Use `--to mac-air` or
`--to archlinux-worker` with `send`, or `--from NAME` with `list` and `get`, to
choose explicitly. Paths containing spaces must be quoted. Dragging a file into
the terminal after `ws-share send ` also supplies its path.

A sent file arrives in the other computer's `~/HiveShare`. To make a local file
available for someone to retrieve with `get`, place a copy in your own `~/HiveShare`.
Existing filenames are never overwritten. Transfers stream in 256 KiB chunks and
verify SHA-256 before publishing the received file. The per-file limit is 1 GiB;
interrupted transfers are discarded and can be retried from the beginning.

`~/.config/ws-share/config.json` is identical on both hosts and contains the peer
addresses and shared secret (mode 600). Keep it private. The per-host launcher sets
`WS_SHARE_MACHINE`. The servers bind only to the configured Tailscale IPs and require
token authentication. They don't expose the rest of your filesystem or execute commands.

The `ws-share` tmux session has a shell window and a `server` window. In Hive's
Sessions page select the machine, open `ws-share`, and use tmux `Ctrl-b n` to switch
windows. CLI access:

```sh
tmux attach -t '=ws-share'
```

The servers last while their tmux session and computer are running; this isn't a
boot service. After reboot, start them again with:

```sh
tmux new-session -d -s ws-share -n shell
tmux new-window -d -t '=ws-share' -n server "$HOME/.local/bin/ws-share serve"
```

If a shell doesn't have `~/.local/bin` on PATH, use `~/.local/bin/ws-share` directly.
Both peers must be online on Tailscale. This doesn't make a sleeping laptop reachable.

Implementation uses the [websockets synchronous API](https://websockets.readthedocs.io/en/15.0.1/reference/sync/server.html),
pinned to 15.0.1 for compatibility with the Air's Python 3.9.

Run the network regression suite in a virtual environment:

```sh
python3 -m venv /tmp/ws-share-test
/tmp/ws-share-test/bin/pip install -r tools/ws-share/requirements.txt
/tmp/ws-share-test/bin/python tools/ws-share/test_ws_share.py
```
