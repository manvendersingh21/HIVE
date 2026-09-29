#!/usr/bin/env python3
"""Acceptance contract: a revived completed run is synced until it is acknowledged.

Delivering a message to a runner only queues a turn in its remote inbox. The turn
emits an `acknowledgment` event carrying that message ID, and the state it ends in
arrives after it, so a run that has just been handed a message still owes Hive
events to import. This check fails if that tracking is removed, if the idle
predicate stops consulting it, or if the test that pins the behaviour is deleted
or weakened. It is a static check so CI can run it on every push; the behaviour
itself is exercised by the Rust test named below.
"""
from pathlib import Path
import re
import sys

ROOT = Path(__file__).resolve().parents[1]
SOURCE = ROOT / 'hive-web/src/delegation.rs'
TEST = 'a_revived_completed_run_is_synced_until_its_message_is_acknowledged'
# The Rust test that proves the behaviour; CI runs it alongside this check.
CARGO_TEST_FILTER = TEST

# Everything the fix is made of, expressed semantically so an unrelated refactor
# of the surrounding store API does not make this check meaningless or noisy.
REQUIRED = {
    'delivered-ID tracking type': r'type\s+Unacknowledged\b',
    'records a delivered ID': r'fn\s+deliver\(\s*unacknowledged:\s*&Unacknowledged',
    'retires an acknowledged ID': r'fn\s+acknowledge\(\s*unacknowledged:\s*&Unacknowledged',
    'idle consults the tracker': r'delivered\.get\(&run\.id\)\.is_none_or\(\|ids\|\s*ids\.is_empty\(\)\)',
    'delivery is marked delivered': r'store\.message_delivered\(',
    'delivered ID is tracked': r'deliver\(\s*unacknowledged,\s*&run\.id,',
    'synced snapshot retires IDs': r'acknowledge\(\s*unacknowledged,\s*&run\.id,\s*&snapshot\)',
    'the acceptance test exists': rf'fn\s+{re.escape(TEST)}\s*\(',
}


def test_body(source: str) -> list[str]:
    """The acceptance test's own lines, stopping at its matching closing brace."""
    lines = source.splitlines()
    for start, line in enumerate(lines):
        if re.search(rf'fn\s+{re.escape(TEST)}\s*\(', line):
            indent = line[:len(line) - len(line.lstrip())]
            body = []
            for line in lines[start:]:
                body.append(line.strip())
                if line == indent + '}':
                    return body
            return body
    return []


def main() -> int:
    source = SOURCE.read_text()
    failures: list[str] = []

    for name, pattern in REQUIRED.items():
        if not re.search(pattern, source):
            failures.append(f'{SOURCE.relative_to(ROOT)} no longer satisfies: {name}')

    # idle() grew a parameter, so every call site has to pass the tracker. A stale
    # two-argument call would compile only if the predicate were a different one.
    stale = len(re.findall(r'idle\(\s*&store,\s*&store\.get\(&id\)\.unwrap\(\)\s*\)', source))
    if stale:
        failures.append(f'{stale} call site(s) still call the pre-fix two-argument idle()')

    # The test must actually pin the behaviour, in order: a delivered message
    # leaves the run non-idle, unrelated events do not retire it, and only the
    # acknowledgment plus a later state make it idle again.
    body = test_body(source)
    if not body:
        failures.append(f'test {TEST} not found in {SOURCE.relative_to(ROOT)}')
    else:
        def follows(needle: str, within: int = 3) -> list[str]:
            for i, line in enumerate(body):
                if needle in line:
                    return body[i + 1:i + 1 + within]
            return []

        if not any('deliver(&unacknowledged, &id,' in line for line in follows('message_delivered')):
            failures.append('the test does not track the delivered message ID')
        if 'assert!(!idle_now());' not in follows('deliver(&unacknowledged, &id, "first")'):
            failures.append('the test no longer asserts a completed run with a delivered message is not idle')
        if 'assert!(!idle_now());' not in follows('message_id":"initial"', 6):
            failures.append('the test no longer asserts an unrelated acknowledgment does not retire the run')
        if 'assert!(idle_now());' not in follows('acknowledge(&unacknowledged, &id, &snapshot)', 5):
            failures.append('the test no longer asserts the run is idle again once acknowledged')
        statements = [line for line in body if line and line not in ('{', '}')]
        if statements[-1] != 'assert!(idle_now());':
            failures.append(f'the test no longer ends on the idle-again assertion (ends: {statements[-1]})')
        if not any('"kind":"acknowledgment"' in line and '"message_id":"first"' in line for line in body):
            failures.append('the test never syncs an acknowledgment of the delivered message')
        if not any('"kind":"state"' in line and '"state":"completed"' in line for line in body):
            failures.append('the test never syncs a later completed state')

    if failures:
        for failure in failures:
            print(f'FAIL: {failure}', flush=True)
        return 1
    print(f'PASS: delivered message IDs keep a completed run synced until acknowledged ({TEST})', flush=True)
    print(f'Rust test: cargo test --locked -p hive-web {CARGO_TEST_FILTER}', flush=True)
    return 0


if __name__ == '__main__':
    sys.exit(main())
