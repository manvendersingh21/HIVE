#!/usr/bin/env python3
"""Authenticated WebSocket file sharing on a private network, with streaming SHA-256 checks."""
import argparse
import hashlib
import hmac
import json
import os
from pathlib import Path
import shutil
import stat
import subprocess
import sys
import tempfile
import threading
from urllib.parse import urlparse

from websockets.sync.client import connect
from websockets.sync.server import serve

CHUNK = 256 * 1024
LIMIT = 1024 * 1024 * 1024
TIMEOUT = 60


def control(ws):
    value = ws.recv(timeout=TIMEOUT)
    if not isinstance(value, str) or len(value) > 16384:
        raise ValueError('Expected a small JSON control message')
    data = json.loads(value)
    if not isinstance(data, dict):
        raise ValueError('Expected a JSON object')
    if 'error' in data:
        raise ValueError(data['error'])
    return data


def send_control(ws, **data):
    ws.send(json.dumps(data))


def filename(name):
    if not isinstance(name, str) or not name or name in ('.', '..') or name.startswith('.ws-part-'):
        raise ValueError('Invalid filename')
    if any(c in name for c in ('/', '\\', '\0')) or len(name.encode()) > 240:
        raise ValueError('Use a filename without directories (maximum 240 bytes)')
    return name


def file_size(size):
    if type(size) is not int or not 0 <= size <= LIMIT:
        raise ValueError('File must be between 0 bytes and 1 GiB')
    return size


def transmit(ws, source, size):
    digest = hashlib.sha256()
    remaining = size
    while remaining:
        data = source.read(min(CHUNK, remaining))
        if not data:
            raise ValueError('Source file changed during transfer')
        ws.send(data)
        digest.update(data)
        remaining -= len(data)
    value = digest.hexdigest()
    send_control(ws, sha256=value)
    return value


def receive(ws, destination, size):
    """Write privately, verify the complete stream, then publish without overwriting."""
    file_size(size)
    if destination.exists() or destination.is_symlink():
        raise FileExistsError('Destination already exists; choose another filename')
    temp = None
    try:
        with tempfile.NamedTemporaryFile(prefix='.ws-part-', dir=destination.parent, delete=False) as out:
            temp = Path(out.name)
            remaining = size
            digest = hashlib.sha256()
            while remaining:
                data = ws.recv(timeout=TIMEOUT)
                if not isinstance(data, bytes) or not 0 < len(data) <= min(CHUNK, remaining):
                    raise ValueError('Invalid file chunk')
                out.write(data)
                digest.update(data)
                remaining -= len(data)
            expected = control(ws).get('sha256', '')
            actual = digest.hexdigest()
            if not isinstance(expected, str) or not hmac.compare_digest(actual, expected):
                raise ValueError('Checksum mismatch; incomplete file discarded')
            out.flush()
            os.fsync(out.fileno())
        os.link(temp, destination)  # Atomic and refuses an existing destination.
        return actual
    finally:
        if temp is not None:
            temp.unlink(missing_ok=True)


def open_regular(path):
    descriptor = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
    stream = os.fdopen(descriptor, 'rb')
    info = os.fstat(stream.fileno())
    if not stat.S_ISREG(info.st_mode):
        stream.close()
        raise ValueError('Only regular files can be shared')
    try:
        file_size(info.st_size)
    except Exception:
        stream.close()
        raise
    return stream, info.st_size


def server(config, machine, root):
    root.mkdir(parents=True, exist_ok=True, mode=0o700)
    address = urlparse(config['machines'][machine])
    slots = threading.BoundedSemaphore(4)

    def auth(connection, request):
        if request.path != '/share':
            return connection.respond(404, 'Not found\n')
        values = request.headers.get_all('Authorization')
        if len(values) != 1 or not hmac.compare_digest(values[0], 'Bearer ' + config['token']):
            return connection.respond(401, 'Authentication required\n')

    def handler(ws):
        acquired = slots.acquire(blocking=False)
        try:
            if not acquired:
                raise ValueError('Server busy; try again shortly')
            request = control(ws)
            action = request.get('action')
            if action == 'ping':
                send_control(ws, machine=machine, ready=True)
            elif action == 'list':
                for path in sorted(root.iterdir()):
                    if not path.name.startswith('.ws-part-') and not path.is_symlink() and path.is_file():
                        send_control(ws, name=path.name, size=path.stat().st_size)
                send_control(ws, done=True)
            elif action == 'put':
                name = filename(request.get('name'))
                size = file_size(request.get('size'))
                destination = root / name
                if destination.exists() or destination.is_symlink():
                    raise FileExistsError('Destination already exists; rename your file first')
                send_control(ws, ready=True)
                digest = receive(ws, destination, size)
                send_control(ws, saved=name, sha256=digest)
                print(f'Received {name!r} ({size} bytes)', flush=True)
            elif action == 'get':
                name = filename(request.get('name'))
                source, size = open_regular(root / name)
                with source:
                    send_control(ws, name=name, size=size)
                    digest = transmit(ws, source, size)
                if control(ws).get('sha256') != digest:
                    raise ValueError('Receiver did not verify the download')
                print(f'Sent {name!r} ({size} bytes)', flush=True)
            else:
                raise ValueError('Unknown action')
        except Exception as error:
            try:
                send_control(ws, error=str(error))
            except Exception:
                pass
        finally:
            if acquired:
                slots.release()

    with serve(handler, address.hostname, address.port, process_request=auth,
               origins=[None], max_size=CHUNK, max_queue=4, compression=None,
               close_timeout=5, open_timeout=10) as listener:
        print(f'{machine}: serving {root} at {config["machines"][machine]}', flush=True)
        listener.serve_forever()


def connection(config, peer):
    return connect(config['machines'][peer], proxy=None,
                   additional_headers={'Authorization': 'Bearer ' + config['token']},
                   compression=None, max_size=CHUNK, max_queue=4, open_timeout=10, close_timeout=5)


def pick_file():
    if sys.platform == 'darwin':
        result = subprocess.run(['osascript', '-e', 'POSIX path of (choose file with prompt "Send a file to the other computer")'], capture_output=True, text=True)
        if result.returncode:
            raise ValueError('File selection cancelled')
        return result.stdout.rstrip('\n')
    if shutil.which('zenity') and (os.environ.get('DISPLAY') or os.environ.get('WAYLAND_DISPLAY')):
        result = subprocess.run(['zenity', '--file-selection'], capture_output=True, text=True)
        if result.returncode:
            raise ValueError('File selection cancelled')
        return result.stdout.rstrip('\n')
    return input('File path to send: ').strip()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--config', type=Path, default=Path.home() / '.config/ws-share/config.json')
    parser.add_argument('--machine', default=os.environ.get('WS_SHARE_MACHINE'))
    parser.add_argument('--directory', type=Path, default=Path.home() / 'HiveShare')
    commands = parser.add_subparsers(dest='command', required=True)
    commands.add_parser('serve')
    commands.add_parser('status')
    send = commands.add_parser('send', help='Select a file or provide its path')
    send.add_argument('file', nargs='?')
    send.add_argument('--to', dest='peer')
    get = commands.add_parser('get', help='Download a file from the other computer’s HiveShare folder')
    get.add_argument('name')
    get.add_argument('destination', nargs='?', default='.')
    get.add_argument('--from', dest='peer')
    listing = commands.add_parser('list')
    listing.add_argument('--from', dest='peer')
    args = parser.parse_args()
    config = json.loads(args.config.read_text())
    if len(config.get('token', '')) < 32:
        raise ValueError('Configuration requires a strong shared token')
    if args.machine not in config['machines']:
        raise ValueError('Set --machine or WS_SHARE_MACHINE to this computer’s configured name')
    if args.command == 'serve':
        return server(config, args.machine, args.directory.expanduser())
    if args.command == 'status':
        failed = False
        for peer in config['machines']:
            try:
                with connection(config, peer) as ws:
                    send_control(ws, action='ping')
                    result = control(ws)
                    if result.get('machine') != peer or not result.get('ready'):
                        raise ValueError('Unexpected server identity')
                    print(f'{peer}: ready')
            except Exception as error:
                failed = True
                print(f'{peer}: unavailable ({error})')
        return 1 if failed else 0
    peer = args.peer or next(name for name in config['machines'] if name != args.machine)
    if peer not in config['machines']:
        raise ValueError(f'Unknown peer: {peer}')
    if args.command == 'send':
        path = Path(args.file or pick_file()).expanduser()
        name = filename(path.name)
        source, size = open_regular(path)
        with source, connection(config, peer) as ws:
            send_control(ws, action='put', name=name, size=size)
            if not control(ws).get('ready'):
                raise ValueError('Receiver was not ready')
            digest = transmit(ws, source, size)
            if control(ws).get('sha256') != digest:
                raise ValueError('Receiver did not verify the upload')
        print(f'Sent {name!r} to {peer}: ~/HiveShare/{name} ({size} bytes, SHA-256 verified)')
    elif args.command == 'get':
        name = filename(args.name)
        destination = Path(args.destination).expanduser()
        if destination.is_dir():
            destination /= name
        with connection(config, peer) as ws:
            send_control(ws, action='get', name=name)
            metadata = control(ws)
            digest = receive(ws, destination, file_size(metadata.get('size')))
            send_control(ws, sha256=digest)
        print(f'Downloaded {name!r} to {destination} (SHA-256 verified)')
    elif args.command == 'list':
        with connection(config, peer) as ws:
            send_control(ws, action='list')
            while True:
                item = control(ws)
                if item.get('done'):
                    break
                print(f'{item["size"]:>12}  {item["name"]!r}')


if __name__ == '__main__':
    try:
        sys.exit(main())
    except (Exception, KeyboardInterrupt) as error:
        print(f'ws-share: {error}', file=sys.stderr)
        sys.exit(1)
