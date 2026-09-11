#!/usr/bin/env python3
"""Exercise actual WebSocket transfers against two disposable local servers."""
import hashlib
import json
import os
from pathlib import Path
import socket
import subprocess
import sys
import tempfile
import time
import unittest

import ws_share as share


class Transfers(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.temp = tempfile.TemporaryDirectory(prefix='ws-share-test-')
        cls.root = Path(cls.temp.name)
        cls.config = {'token': 'test-only-' + 'a' * 48, 'machines': {}}
        cls.servers = []
        for name in ['air', 'arch']:
            with socket.socket() as sock:
                sock.bind(('127.0.0.1', 0))
                port = sock.getsockname()[1]
            cls.config['machines'][name] = f'ws://127.0.0.1:{port}/share'
        cls.config_path = cls.root / 'config.json'
        cls.config_path.write_text(json.dumps(cls.config))
        for name in cls.config['machines']:
            proc = subprocess.Popen([sys.executable, share.__file__, '--config', str(cls.config_path),
                '--machine', name, '--directory', str(cls.root / name), 'serve'], stdout=subprocess.DEVNULL)
            cls.servers.append(proc)
            for _ in range(100):
                try:
                    with share.connection(cls.config, name) as ws:
                        share.send_control(ws, action='ping')
                        assert share.control(ws)['machine'] == name
                    break
                except OSError:
                    time.sleep(.02)
            else:
                raise RuntimeError('Server did not start')

    @classmethod
    def tearDownClass(cls):
        for proc in cls.servers:
            proc.terminate()
            proc.wait(timeout=5)
        cls.temp.cleanup()

    def cli(self, machine, *args, ok=True):
        result = subprocess.run([sys.executable, share.__file__, '--config', str(self.config_path),
            '--machine', machine, *map(str, args)], capture_output=True, text=True)
        self.assertEqual(result.returncode == 0, ok, result.stdout + result.stderr)
        return result.stdout

    def test_binary_unicode_spaces_bidirectional_and_no_overwrite(self):
        data = os.urandom(share.CHUNK * 3 + 713)
        source = self.root / 'résumé with spaces.bin'
        source.write_bytes(data)
        self.cli('air', 'send', source)
        self.assertEqual((self.root / 'arch' / source.name).read_bytes(), data)
        self.assertIn(source.name, self.cli('air', 'list'))
        target = self.root / 'download.bin'
        self.cli('air', 'get', source.name, target)
        self.assertEqual(target.read_bytes(), data)
        self.cli('air', 'get', source.name, target, ok=False)
        self.cli('air', 'send', source, ok=False)
        self.cli('arch', 'send', source)
        self.assertEqual((self.root / 'air' / source.name).read_bytes(), data)

    def test_empty_file(self):
        path = self.root / 'empty'
        path.touch()
        self.cli('air', 'send', path)
        self.cli('air', 'get', 'empty', self.root / 'empty-downloaded')
        self.assertEqual((self.root / 'empty-downloaded').read_bytes(), b'')

    def test_auth_traversal_symlink_size_and_checksum(self):
        bad = dict(self.config, token='wrong-token')
        with self.assertRaises(Exception):
            with share.connection(bad, 'air'):
                self.fail('Authentication bypass')
        for request in [dict(action='put', name='../escape', size=0),
                        dict(action='put', name='too-large', size=share.LIMIT + 1)]:
            with share.connection(self.config, 'air') as ws:
                share.send_control(ws, **request)
                with self.assertRaises(ValueError):
                    share.control(ws)
        secret = self.root / 'outside'
        secret.write_text('outside sharing directory')
        (self.root / 'air' / 'link').symlink_to(secret)
        self.cli('arch', 'get', 'link', self.root / 'leaked', ok=False)
        self.assertFalse((self.root / 'leaked').exists())
        with share.connection(self.config, 'air') as ws:
            share.send_control(ws, action='put', name='corrupt', size=3)
            self.assertTrue(share.control(ws)['ready'])
            ws.send(b'abc')
            share.send_control(ws, sha256='0' * 64)
            with self.assertRaises(ValueError):
                share.control(ws)
        self.assertFalse((self.root / 'air' / 'corrupt').exists())
        self.assertFalse(list((self.root / 'air').glob('.ws-part-*')))

    def test_disconnect_discards_partial_file(self):
        with share.connection(self.config, 'air') as ws:
            share.send_control(ws, action='put', name='partial', size=6)
            share.control(ws)
            ws.send(b'abc')
        for _ in range(100):
            if not list((self.root / 'air').glob('.ws-part-*')):
                break
            time.sleep(.01)
        self.assertFalse((self.root / 'air' / 'partial').exists())
        self.assertFalse(list((self.root / 'air').glob('.ws-part-*')))


if __name__ == '__main__':
    unittest.main()
