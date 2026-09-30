"""Delete only allowlisted cache directories under a direct workspace child.

Never follow symlinks (including the workspace root). Input arrives over the
existing transport's stdin; no worker-provided text is interpolated into shell.
"""
import json
import os
from pathlib import Path
import shutil
import sys
import sqlite3
import subprocess
import uuid

CACHE_NAMES = {'target', 'node_modules', '.next', 'playwright-report', 'playwright-results', 'test-results'}


def collect(request):
    root = Path.home() / 'hive-workspaces'
    workspace = Path(os.path.expanduser(request['workspace']))
    if root.is_symlink() or workspace.is_symlink():
        raise ValueError('symlink workspace/root')
    if workspace.parent != root or workspace.name in ('', '.', '..'):
        raise ValueError('workspace must be a direct child of ~/hive-workspaces')
    root = root.resolve()
    workspace = workspace.resolve()
    if workspace.parent != root:
        raise ValueError('workspace escapes root')
    for name in request.get('live', []):
        live = Path(os.path.expanduser(name)).resolve()
        if live == workspace or workspace in live.parents or live in workspace.parents:
            raise ValueError('workspace overlaps a live run')
    removed = []
    freed = 0
    for parent, dirs, _ in os.walk(workspace, followlinks=False):
        for name in list(dirs):
            path = Path(parent) / name
            if path.is_symlink() or name == '.git':
                dirs.remove(name)
                continue
            if name not in CACHE_NAMES:
                continue
            size = 0
            for base, children, files in os.walk(path, followlinks=False):
                children[:] = [n for n in children if not (Path(base) / n).is_symlink()]
                for item in files:
                    entry = Path(base) / item
                    if not entry.is_symlink():
                        size += entry.stat().st_blocks * 512
            shutil.rmtree(path)
            removed.append(str(path.relative_to(workspace)))
            freed += size
            dirs.remove(name)
    return {'removed': removed, 'freed_bytes': freed}


if __name__ == '__main__':
    request = json.load(sys.stdin)
    db = None
    try:
        if request.get('journal'):
            ident = str(uuid.UUID(request['run_id']))
            path = Path.home() / '.hive' / 'runs' / ident / 'journal.db'
            db = sqlite3.connect(path.as_uri() + '?mode=rw', uri=True, timeout=5)
            db.execute('BEGIN IMMEDIATE')
            if request.get('superseded'):
                # Replaced workers must have stopped, even if their final journal
                # state still says working. Missing tmux fails closed.
                session = subprocess.run(['tmux', 'has-session', '-t', '=' + request['tmux']],
                                         stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
                if session.returncode != 1:
                    raise ValueError('superseded session still active or unknown')
            else:
                state = db.execute("SELECT value FROM metadata WHERE key='state'").fetchone()
                pending = db.execute("SELECT 1 FROM inbox WHERE state IN ('queued','delivering')").fetchone()
                if not state or json.loads(state[0]) not in ('completed', 'failed') or (json.loads(state[0]) == 'completed' and pending):
                    raise ValueError('runner is live or has pending work')
        print(json.dumps(collect(request)))
    finally:
        if db is not None:
            db.rollback()
            db.close()
