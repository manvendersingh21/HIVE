"""Private SSH host plumbing. Not a protocol implementation or an agent driver.

One bounded JSON request on stdin, one JSON response on stdout. All artifact bytes
are measured at each end. Nothing installs a daemon or modifies shell profiles.
"""
import hashlib
import json
import os
from pathlib import Path
import re
import signal
import subprocess
import sys
import time

LIMIT = 32 * 1024 * 1024
FILES = 2048


def checked(root, relative):
    assert isinstance(relative, str) and relative and "\\" not in relative
    parts = relative.split("/")
    assert all(p not in ("", ".", "..") for p in parts), "unsafe relative path"
    path = root
    for part in parts:
        path = path / part
        assert not path.is_symlink(), "symlink in transfer path"
    assert path.resolve().is_relative_to(root.resolve()), "path escaped workspace"
    return path


def digest(data):
    return hashlib.sha256(data).hexdigest()


def snapshot(root):
    files = []
    total = 0
    for parent, dirs, names in os.walk(root, followlinks=False):
        dirs[:] = sorted(d for d in dirs if d not in (".git", "__pycache__"))
        for name in dirs + names:
            assert not (Path(parent) / name).is_symlink(), "symlinks are not transferable"
        for name in sorted(names):
            path = Path(parent) / name
            assert path.is_file(), "only regular files are transferable"
            assert path.stat().st_size <= LIMIT, "file exceeds transfer limit"
            data = path.read_bytes()
            total += len(data)
            assert total <= LIMIT and len(files) < FILES, "workspace exceeds transfer limits"
            files.append({"path": str(path.relative_to(root)), "bytes": list(data),
                          "digest": digest(data), "executable": bool(path.stat().st_mode & 0o111)})
    return files


def install(root, files):
    assert len(files) <= FILES
    total = 0
    prepared = []
    seen = set()
    for record in files:
        path = checked(root, record["path"])
        assert record["path"] not in seen, "duplicate transfer path"
        seen.add(record["path"])
        data = bytes(record["bytes"])
        total += len(data)
        assert total <= LIMIT and digest(data) == record["digest"], "transfer digest/size mismatch"
        prepared.append((path, data, record["executable"]))
    # Verify the complete manifest before changing any destination file.
    for path, data, executable in prepared:
        path.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
        temporary = path.with_name("." + path.name + ".hive-transfer")
        assert not temporary.is_symlink()
        fd = os.open(temporary, os.O_CREAT | os.O_TRUNC | os.O_WRONLY | os.O_NOFOLLOW, 0o600)
        with os.fdopen(fd, "wb") as stream:
            stream.write(data)
            stream.flush()
            os.fsync(stream.fileno())
        os.chmod(temporary, 0o700 if executable else 0o600)
        os.replace(temporary, path)
    return [{"path": r["path"], "digest": r["digest"]} for r in files]


def tmux(*args):
    return subprocess.run(["tmux", *args], stdin=subprocess.DEVNULL,
                          stdout=subprocess.PIPE, stderr=subprocess.PIPE, check=False)


def exists(name):
    result = tmux("has-session", "-t", "=" + name)
    if result.returncode == 0:
        return True
    error = result.stderr.decode(errors="replace")
    # Transport errors happen outside this process and must never become 'absent'.
    assert "can't find session" in error or "no server running" in error or "No such file" in error, error
    return False


def pane(name, field):
    result = tmux("display-message", "-p", "-t", "=" + name + ":", "#{" + field + "}")
    assert result.returncode == 0, result.stderr.decode(errors="replace")
    return result.stdout.decode().strip()


def processes(tty):
    result = subprocess.run(["ps", "-t", tty.removeprefix("/dev/"), "-o", "pgid=,stat="],
                            stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, check=True)
    return [(int(fields[0]), fields[1]) for line in result.stdout.decode().splitlines()
            if len(fields := line.split()) == 2]


def main(req):
    root = Path(req["root"])
    assert root.is_absolute() and root.name.startswith("hive-collab-"), "invalid run root"
    assert not root.is_symlink()
    root.mkdir(mode=0o700, parents=True, exist_ok=True)
    mode = req["mode"]
    if mode == "preflight":
        result = tmux("-V")
        assert result.returncode == 0
        return {"tmux": result.stdout.decode().strip(), "python": sys.version.split()[0]}
    if mode in ("put", "get"):
        workspace = checked(root, req["workspace"])
        workspace.mkdir(mode=0o700, parents=True, exist_ok=True)
        if mode == "put":
            return {"received": install(workspace, req["files"])}
        return {"files": snapshot(workspace)}
    name = req["name"]
    assert re.fullmatch(r"hive-[A-Za-z0-9_-]+", name), "invalid session name"
    log = checked(root, req["log"])
    if mode == "launch":
        assert not exists(name), "session already exists"
        log.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
        assert not log.exists(), "log already exists; recover, do not relaunch"
        log.touch(mode=0o600)
        cwd = checked(root, req["workspace"])
        # tmux rejects long command messages well below the OS argv limit. The
        # original invocation stays intact in a private, non-overwritten script.
        script = log.with_suffix(".launch.sh")
        fd = os.open(script, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600)
        with os.fdopen(fd, "w") as stream:
            stream.write(req["command"])
            stream.flush()
            os.fsync(stream.fileno())
        result = tmux("new-session", "-d", "-s", name, "-c", str(cwd), "-e", "PATH=" + os.environ["PATH"],
                      "bash", "-l", str(script))
        assert result.returncode == 0, result.stderr.decode(errors="replace")
        return {"launched": True}
    if mode == "poll":
        live = exists(name)
        offset = req.get("offset", 0)
        assert isinstance(offset, int) and offset >= 0
        assert log.exists() or not live, "live session lost its log"
        data = b""
        size = 0
        if log.exists():
            size = log.stat().st_size
            assert offset <= size, "remote log was truncated"
            with log.open("rb") as stream:
                stream.seek(offset)
                data = stream.read(65536)
        return {"live": live, "bytes": list(data), "size": size,
                "log_exists": log.exists(), "drained": offset + len(data) == size}
    if mode in ("pause", "resume"):
        if not exists(name):
            return {"ended": True}
        pid = int(pane(name, "pane_pid"))
        tty = pane(name, "pane_tty")
        rows = processes(tty)
        if mode == "pause":
            result = subprocess.check_output(["ps", "-o", "tpgid=", "-p", str(pid)], stdin=subprocess.DEVNULL)
            group = int(result.strip())
            assert group > 1 and group not in (pid, os.getpgrp()), "unsafe foreground process group"
            assert any(g == group for g, _ in rows), "foreground group is not on the pane tty"
            os.killpg(group, signal.SIGSTOP)
            time.sleep(0.2)
            assert any(g == group and s.startswith("T") for g, s in processes(tty)), "SIGSTOP did not hold"
            return {"stopped": True}
        groups = {g for g, state in rows if state.startswith("T")}
        for group in groups:
            assert group > 1 and group not in (pid, os.getpgrp()), "unsafe stopped process group"
            os.killpg(group, signal.SIGCONT)
        return {"resumed": len(groups)}
    raise ValueError("unknown host operation")


if __name__ == "__main__":
    try:
        request = json.load(sys.stdin)
        print(json.dumps(main(request), separators=(",", ":")))
    except Exception as error:
        print(str(error), file=sys.stderr)
        sys.exit(1)
