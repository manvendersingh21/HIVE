#!/usr/bin/env python3
"""Mechanical acceptance gate for the Cursor delegated agent.

Every check here is one acceptance criterion, evaluated offline: no network, no
model calls, no worker CLIs and no user configuration are read, so the result
depends only on the tree being checked. The script exits non-zero as soon as a
check fails, which lets CI measure the feature instead of trusting a report.

    python3 scripts/check-cursor-agent.py             wiring and adapter behaviour
    python3 scripts/check-cursor-agent.py --frontend  also run the browser spec
"""
from __future__ import annotations

import argparse
import os
import re
import socket
import subprocess
import sys
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
RUNNER_DIR = ROOT / "hive-core" / "src" / "delegation" / "runner"
FRONTEND = ROOT / "hive-web" / "frontend"

# Every documented flag a headless turn needs. Losing one means the build cannot
# run delegated work, which is why the probe refuses to call itself ready.
REQUIRED_FLAGS = (
    "--print",
    "--output-format",
    "stream-json",
    "--trust",
    "--workspace",
    "--force",
    "--resume",
    "--model",
)

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument(
    "--frontend",
    action="store_true",
    help="also run the runEvents browser spec (needs hive-web/frontend/node_modules)",
)
args = parser.parse_args()


class Failed(Exception):
    """A check did not hold."""


def require(condition, message):
    if not condition:
        raise Failed(message)


def read(relative):
    return (ROOT / relative).read_text(encoding="utf-8")


def check_runner_registration():
    """The runner knows the type, the binary and the adapter."""
    source = read("hive-core/src/delegation/runner/runner.py")
    agents = re.search(r"^AGENTS = (.+)$", source, re.M)
    require(agents is not None, "runner.py has no AGENTS tuple")
    require(
        "cursor" in [name.strip().strip("\"'") for name in agents.group(1).strip("()").split(",")],
        "AGENTS does not accept the cursor type: " + agents.group(0),
    )
    require(
        re.search(r"^EXECUTABLES = \{'cursor': 'cursor-agent'\}$", source, re.M) is not None,
        "EXECUTABLES does not map the cursor type onto the cursor-agent binary",
    )
    require(
        "name = EXECUTABLES.get(name, name)" in source,
        "executable() ignores EXECUTABLES, so the cursor type would look for a 'cursor' binary",
    )
    require("'cursor': Cursor" in source, "the adapter registry does not construct Cursor")
    require("elif name == 'cursor':" in source, "the probe has no cursor branch")


def check_probe_measures_the_cli():
    """The probe reports version, readiness, authentication and models."""
    source = read("hive-core/src/delegation/runner/runner.py")
    require("elif name == 'cursor':" in source, "the probe has no cursor branch")
    branch = source.split("elif name == 'cursor':", 1)[1].split("\n            elif", 1)[0]
    for flag in REQUIRED_FLAGS:
        require(flag in branch, "the cursor probe never checks the " + flag + " flag")
    # A textual match alone would survive `all(...)` becoming `any(...)`, which
    # would report a CLI missing flags as ready. test_adapters exercises the
    # real condition; this only pins the shape that test depends on.
    require(
        re.search(r"runtime_ready'\] = all\(flag in help_text for flag in \(", branch) is not None,
        "the cursor probe does not require every documented flag to be present",
    )
    require("record['runtime_ready']" in branch, "the cursor probe never reports runtime_ready")
    require("record['authentication']" in branch, "the cursor probe never reports authentication")
    require("record['models']" in branch, "the cursor probe never lists models")
    require("--version" in source, "the probe never records a version")


def check_adapter_contracts():
    """The cursor adapter contract tests, run for real against a fake CLI."""
    command = [sys.executable, "-m", "unittest", "-k", "cursor", "test_adapters"]
    finished = subprocess.run(
        command, cwd=RUNNER_DIR, capture_output=True, text=True, timeout=600
    )
    output = finished.stdout + finished.stderr
    ran = re.search(r"Ran (\d+) tests?", output)
    require(ran is not None, "the cursor adapter tests did not run:\n" + output[-2000:])
    require(
        int(ran.group(1)) >= 9,
        "expected the 9 documented cursor adapter tests, ran " + ran.group(1),
    )
    require(
        output.rstrip().endswith("OK"),
        "the cursor adapter contract tests failed:\n" + output[-2000:],
    )
    return "adapter contract tests passed (%s)" % ran.group(1)


def check_planner_accepts_cursor():
    """The planner validates, routes and offers the type in its schema."""
    source = read("hive-core/src/delegation/mod.rs")
    require(
        re.search(r'\["claude", "codex", "agy", "opencode", "cursor"\]\.contains\(&a\.agent', source)
        is not None,
        "validate() does not accept the cursor type",
    )
    require(
        re.search(r'opencode\|cursor\)\(\?:\\s\+agent\)', source) is not None,
        "validate_explicit() does not recognise 'cursor on <device>'",
    )
    require(
        '"agent":{"enum":["claude","codex","agy","opencode","cursor"]}' in source,
        "the planner JSON schema does not offer the cursor type",
    )
    require(
        "fn cursor_assignments_validate_and_honour_explicit_placements()" in source,
        "the cursor planner regression test is missing",
    )


def check_worker_probe_advertises_cursor():
    """Container workers report the binary so a device without it is visible."""
    machines = read("hive-core/src/memory/machines.rs")
    require(
        re.search(r'^\s*"cursor-agent",\s*$', machines, re.M) is not None,
        "PROBED_TOOLS does not probe cursor-agent",
    )
    containers = read("hive-web/src/containers.rs")
    require(
        re.search(r"const CHECK: &str = .*\bcursor-agent\b", containers) is not None,
        "the container dependency check does not look for cursor-agent",
    )
    require(
        re.search(r'\["claude", "codex", "opencode", "cursor-agent"\]', containers) is not None,
        "the container agent list does not include cursor-agent",
    )


def check_frontend_maps_cursor_events():
    """The event renderer understands the stream-json shapes Cursor emits."""
    source = read("hive-web/frontend/lib/runEvents.ts")
    require("const cursorTool = " in source, "runEvents.ts cannot unwrap a cursor tool_call")
    require(
        "const CURSOR_EDIT_TOOLS = " in source,
        "runEvents.ts does not know which cursor tools are file edits",
    )
    require(
        "function cursorToolCall(" in source,
        "runEvents.ts has no renderer for cursor tool calls",
    )
    require(
        re.search(r'p\.type === "thinking" && p\.subtype === "delta"', source) is not None,
        "runEvents.ts does not join cursor thinking deltas into one entry",
    )
    require(
        re.search(r'cursorToolCall\(out, e\.seq, p\)', source) is not None,
        "runEvents.ts never dispatches a cursor tool_call event",
    )
    require(
        "str(p.call_id)" in source,
        "runEvents.ts does not correlate a cursor call with its completion",
    )


def check_frontend_spec_pins_cursor():
    """The nine documented cursor rendering behaviours stay pinned."""
    source = read("hive-web/frontend/tests/runEvents.spec.ts")
    heading = 'test.describe("cursor-agent stream-json events"'
    require(heading in source, "the runEvents spec has no cursor-agent block")
    block = source.split(heading, 1)[1].split("\ntest.describe(", 1)[0]
    tests = re.findall(r'^\s{2}test\(', block, re.M)
    require(
        len(tests) >= 9,
        "expected 9 cursor rendering tests, found %d" % len(tests),
    )
    # Counting declarations is not enough on its own: a skipped or focused test
    # keeps its `test(` line while its assertions stop running.
    for pattern, label in (
        (r"^\s*test\.(?:skip|fixme)\(", "a skipped"),
        (r"^\s*(?:test|test\.describe)\.only\(", "a focused"),
    ):
        require(
            re.search(pattern, block, re.M) is None,
            "%s cursor rendering test would not run" % label,
        )
    return "%d cursor rendering tests pinned" % len(tests)


def port_free(port=18081):
    with socket.socket() as probe:
        probe.settimeout(0.5)
        return probe.connect_ex(("127.0.0.1", port)) != 0


def wait_for_port(free, seconds):
    """Playwright tears its static server down asynchronously; wait it out."""
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline:
        if port_free() == free:
            return True
        time.sleep(0.25)
    return port_free() == free


def check_frontend_spec_runs():
    """Run the cursor block through the real test runner."""
    if not (FRONTEND / "node_modules").is_dir():
        raise Failed(
            "hive-web/frontend/node_modules is absent; run 'npm ci' in that directory first"
        )
    # The Playwright config serves the static export, so an unbuilt frontend
    # otherwise shows up as a 60s webServer timeout instead of a clear cause.
    if not (FRONTEND / "out").is_dir():
        raise Failed(
            "hive-web/frontend/out is absent; run 'npm run build' in that directory first"
        )
    # The Playwright config pins port 18081 with reuseExistingServer off, so a
    # server left behind by an earlier run would fail this check spuriously.
    if not wait_for_port(free=True, seconds=15):
        raise Failed("port 18081 is still in use; another Playwright run is active")
    finished = subprocess.run(
        ["npx", "playwright", "test", "tests/runEvents.spec.ts"],
        cwd=FRONTEND,
        capture_output=True,
        text=True,
        timeout=1800,
        env=dict(os.environ, CI="true"),
    )
    wait_for_port(free=True, seconds=30)
    output = finished.stdout + finished.stderr
    require(
        finished.returncode == 0,
        "the runEvents spec failed:\n" + output[-2000:],
    )
    passed = re.search(r"(\d+) passed", output)
    return "%s runEvents tests passed" % (passed.group(1) if passed else "all")


CHECKS = (
    ("runner registers the cursor type and its binary", check_runner_registration),
    ("probe measures the cursor CLI", check_probe_measures_the_cli),
    ("cursor adapter contract tests", check_adapter_contracts),
    ("planner accepts and routes cursor", check_planner_accepts_cursor),
    ("container workers probe cursor-agent", check_worker_probe_advertises_cursor),
    ("frontend maps cursor events", check_frontend_maps_cursor_events),
    ("frontend spec pins cursor rendering", check_frontend_spec_pins_cursor),
)
if args.frontend:
    CHECKS += (("frontend spec passes", check_frontend_spec_runs),)

failures = 0
for name, check in CHECKS:
    start = time.monotonic()
    try:
        detail = check()
    except Failed as error:
        failures += 1
        print(f"{name}: FAIL ({time.monotonic() - start:.2f}s): {error}", flush=True)
    except Exception as error:  # noqa: BLE001 - a crashing check is a failing check
        failures += 1
        print(
            f"{name}: FAIL ({time.monotonic() - start:.2f}s): "
            f"{type(error).__name__}: {error}",
            flush=True,
        )
    else:
        suffix = f" ({detail})" if detail else ""
        print(f"{name}: PASS ({time.monotonic() - start:.2f}s){suffix}", flush=True)

print(f"Cursor acceptance checks run: {len(CHECKS)}")
if failures:
    print(f"Cursor acceptance checks failed: {failures}")
    sys.exit(1)
print("Cursor acceptance checks passed.")
