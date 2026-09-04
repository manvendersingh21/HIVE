#!/usr/bin/env python3
"""An independent HACP/2.0 peer — party B of a bilateral lifecycle, over the
file edge (spec/HACP-2.0-draft.md §12.1).

INFORMATION BARRIER (the point of this file): this implementation reads only
  * hacp/spec/HACP-2.0-draft.md        (normative semantics)
  * hacp/spec/schemas/*.json           (canonical wire schemas)
  * hacp/tests/golden/*.jsonl          (worked examples)
and imports neither HIVE nor the reference Rust crate. It is Python stdlib
only. If interoperation with the reference peer required reading the reference
source, that would be a spec defect, and gets reported as one.

Lifecycle implemented (§6, §7, §9): receive session.open, declare features,
receive a contract proposal, accept, receive the freeze — recomputing the
revision digest independently (§7.5) — perform the trivial work, deliver a
submission with an artifact manifest, and receive the verdict, applying the
§9.4 rule: an accept must carry subject artifacts and a passing check.
"""

import argparse
import hashlib
import json
import os
import re
import sys
import time
import uuid
from datetime import datetime, timezone

PROTOCOL = "HACP/2.0"
AGENT_URN_RE = re.compile(r"^urn:hacp:agent:[a-z0-9._-]{1,64}$")
MESSAGE_ID_RE = re.compile(r"^m-[0-9a-f]{12,}$")
TIMESTAMP_RE = re.compile(r"^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}Z$")


class ProtocolError(Exception):
    pass


# --------------------------------------------------------------------------
# Canonical form (§5.1) — independent implementation of the same rules.
# --------------------------------------------------------------------------

def canonical(value):
    out = []
    _write(value, out)
    return "".join(out)


def _write(value, out):
    if value is None:
        out.append("null")
    elif value is True:
        out.append("true")
    elif value is False:
        out.append("false")
    elif isinstance(value, int):
        out.append(str(value))
    elif isinstance(value, float):
        raise ProtocolError("canonical form carries integers only (§5.1)")
    elif isinstance(value, str):
        _write_string(value, out)
    elif isinstance(value, list):
        out.append("[")
        for i, item in enumerate(value):
            if i:
                out.append(",")
            _write(item, out)
        out.append("]")
    elif isinstance(value, dict):
        out.append("{")
        for i, (key, item) in enumerate(sorted(value.items(), key=lambda kv: kv[0].encode("utf-8"))):
            if i:
                out.append(",")
            _write_string(key, out)
            out.append(":")
            _write(item, out)
        out.append("}")
    else:
        raise ProtocolError(f"unserializable value {value!r}")


def _write_string(s, out):
    out.append('"')
    for ch in s:
        if ch == '"':
            out.append('\\"')
        elif ch == "\\":
            out.append("\\\\")
        elif ord(ch) < 0x20:
            out.append(f"\\u{ord(ch):04x}")
        else:
            out.append(ch)
    out.append('"')


def digest_of(value):
    return hashlib.sha256(canonical(value).encode("utf-8")).hexdigest()


def now():
    return datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


def new_message_id():
    return "m-" + uuid.uuid4().hex[:24]


# --------------------------------------------------------------------------
# Envelope validation (§5.2) + a minimal consumer for the committed schemas.
# --------------------------------------------------------------------------

def validate_envelope(env):
    def bad(why):
        raise ProtocolError(f"invalid envelope: {why}")
    if env.get("protocol") != PROTOCOL:
        bad(f"protocol {env.get('protocol')!r}")
    if not MESSAGE_ID_RE.match(env.get("message_id", "")):
        bad(f"message_id {env.get('message_id')!r}")
    if not env.get("session_id"):
        bad("session_id")
    if not AGENT_URN_RE.match(env.get("from", "")):
        bad(f"from {env.get('from')!r}")
    if not AGENT_URN_RE.match(env.get("to", "")):
        bad(f"to {env.get('to')!r}")
    if not env.get("kind"):
        bad("kind")
    if not TIMESTAMP_RE.match(env.get("timestamp", "")):
        bad(f"timestamp {env.get('timestamp')!r}")
    reply = env.get("in_reply_to")
    if reply is not None and not MESSAGE_ID_RE.match(reply):
        bad(f"in_reply_to {reply!r}")
    try:
        canonical(env.get("body"))
    except ProtocolError as e:
        bad(f"body: {e}")


def check_against_schema(env, schema):
    """Minimal draft-07 consumer: required keys and primitive types. Proves
    the committed schema files are usable standalone; not a full validator."""
    required = schema.get("required", [])
    for key in required:
        if key not in env:
            raise ProtocolError(f"schema: missing required {key!r}")
    props = schema.get("properties", {})
    for key, declared in props.items():
        if key not in env:
            continue
        want = declared.get("type")
        got = env[key]
        ok = {
            "string": lambda v: isinstance(v, str),
            "integer": lambda v: isinstance(v, int) and not isinstance(v, bool),
            "object": lambda v: isinstance(v, dict),
            "array": lambda v: isinstance(v, list),
        }.get(want, lambda v: True)(got)
        if not ok:
            raise ProtocolError(f"schema: {key!r} should be {want}")


# --------------------------------------------------------------------------
# The peer.
# --------------------------------------------------------------------------

class Peer:
    def __init__(self, edge, self_urn, peer_urn, schemas):
        self.a_out = os.path.join(edge, "a-out")   # reference -> peer
        self.b_out = os.path.join(edge, "b-out")   # peer -> reference
        self.workspace = os.path.join(edge, "workspace")
        for d in (self.a_out, self.b_out, self.workspace):
            os.makedirs(d, exist_ok=True)
        self.self_urn = self_urn
        self.peer_urn = peer_urn
        self.session_id = None
        self.contract_id = None
        self.terms = None
        self.revision_digest = None
        self.counter = 0
        self.frames = []
        with open(os.path.join(schemas, "envelope.json"), encoding="utf-8") as f:
            self.envelope_schema = json.load(f)

    # -- file edge ---------------------------------------------------------

    def poll_new(self):
        seen = getattr(self, "_seen", set())
        self._seen = seen
        names = sorted(n for n in os.listdir(self.a_out) if n.endswith(".json") and n not in seen)
        out = []
        for name in names:
            seen.add(name)
            with open(os.path.join(self.a_out, name), encoding="utf-8") as f:
                out.append((name, json.load(f)))
        return out

    def emit(self, kind, body):
        self.counter += 1
        env = {
            "protocol": PROTOCOL,
            "message_id": new_message_id(),
            "session_id": self.session_id,
            "from": self.self_urn,
            "to": self.peer_urn,
            "kind": kind,
            "timestamp": now(),
            "body": body,
        }
        validate_envelope(env)
        check_against_schema(env, self.envelope_schema)
        name = f"{self.counter:03d}-{kind}.json"
        with open(os.path.join(self.b_out, name), "w", encoding="utf-8") as f:
            json.dump(env, f)
        self.frames.append({"dir": "b>a", "envelope": env})
        return env

    def absorb(self, env):
        validate_envelope(env)
        check_against_schema(env, self.envelope_schema)
        if env["from"] != self.peer_urn or env["to"] != self.self_urn:
            raise ProtocolError(f"envelope not for me: {env['from']} -> {env['to']}")
        self.frames.append({"dir": "a>b", "envelope": env})

    # -- the lifecycle -----------------------------------------------------

    def run(self, idle_timeout=60.0):
        deadline = time.time() + idle_timeout
        while time.time() < deadline:
            for _, env in self.poll_new():
                self.handle(env)
                deadline = time.time() + idle_timeout
                if self.done:
                    return
            time.sleep(0.05)
        raise ProtocolError("idle timeout before the lifecycle completed")

    @property
    def done(self):
        return self.revision_digest is not None and any(
            f["envelope"]["kind"] == "verification.delivered" for f in self.frames
        )

    def handle(self, env):
        kind = env["kind"]
        if kind == "session.open":
            self.session_id = env["session_id"]
            self.absorb(env)
            self.emit("session.features", {
                "features": ["delegation", "artifact-digest", "observer-events"],
            })
        elif kind == "session.features":
            self.absorb(env)
        elif kind == "contract.proposed":
            self.absorb(env)
            body = env["body"]
            self.contract_id = body["contract_id"]
            self.terms = body["terms"]
            self.emit("contract.accepted", {"contract_id": self.contract_id, "accepted": True})
        elif kind == "contract.accepted":
            self.absorb(env)
        elif kind == "contract.frozen":
            self.absorb(env)
            self.frozen(env["body"])
        elif kind == "verification.delivered":
            self.absorb(env)
            self.verdict(env["body"])
        else:
            # §5.3: unknown kinds are delivered, never rejected. Recorded.
            self.absorb(env)

    def frozen(self, body):
        # §7.5: recompute the revision digest independently. The preimage is
        # the canonical form of {contract_id, revision, content}.
        recomputed = digest_of({
            "contract_id": body["contract_id"],
            "revision": body["revision"],
            "content": self.terms,
        })
        if recomputed != body["digest"]:
            raise ProtocolError(
                f"revision digest mismatch: peer computed {recomputed}, "
                f"reference claimed {body['digest']}"
            )
        self.revision_digest = body["digest"]
        # EXECUTE is a state, not a message (§7.5): work begins now, silently.
        content = "one line of honest work\n"
        artifact_path = os.path.join(self.workspace, "thing.txt")
        with open(artifact_path, "w", encoding="utf-8") as f:
            f.write(content)
        artifact_id = f"urn:hacp:artifact:{uuid.uuid4()}"
        raw = content.encode("utf-8")
        self.emit("submission.delivered", {
            "contract_id": self.contract_id,
            "against_revision": self.revision_digest,
            "artifacts": [artifact_id],
            "artifacts_info": [{
                "artifact_id": artifact_id,
                "media_type": "text/plain",
                "digest": hashlib.sha256(raw).hexdigest(),
                "size": len(raw),
                "producer": self.self_urn,
                "task_id": "t-000000000003",
                "contract_id": self.contract_id,
                "contract_revision": self.revision_digest,
                "derived_from": [],
                "location": "workspace/thing.txt",
                "visibility": "participants",
            }],
            "evidence": [],
            "claim": "thing.txt written with one non-empty line",
        })

    def verdict(self, body):
        # §9.4, mirrored: an accept on signals alone is refused by this peer too.
        if body.get("verdict") == "accept":
            if not body.get("artifacts"):
                raise ProtocolError("accept without subject artifacts (§9.4)")
            checks = body.get("checks", [])
            if not any(c.get("passed") for c in checks):
                raise ProtocolError("accept without a passing check (§9.4)")


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--edge-dir", required=True)
    ap.add_argument("--self-urn", default="urn:hacp:agent:child-1")
    ap.add_argument("--peer-urn", default="urn:hacp:agent:parent-1")
    ap.add_argument("--schemas", required=True)
    args = ap.parse_args()

    peer = Peer(args.edge_dir, args.self_urn, args.peer_urn, args.schemas)
    try:
        peer.run()
    except ProtocolError as e:
        print(f"peer failed: {e}", file=sys.stderr)
        return 1

    # Write the peer's view for mutual-transcript comparison.
    with open(os.path.join(args.edge_dir, "peer-transcript.jsonl"), "w", encoding="utf-8") as f:
        for frame in peer.frames:
            f.write(canonical(frame) + "\n")
    with open(os.path.join(args.edge_dir, "peer-digest.txt"), "w", encoding="utf-8") as f:
        f.write(peer.revision_digest or "")
    with open(os.path.join(args.edge_dir, "peer-status.json"), "w", encoding="utf-8") as f:
        f.write(canonical({"outcome": "complete", "frames": len(peer.frames)}))
    print(f"peer complete: {len(peer.frames)} frames")
    return 0


if __name__ == "__main__":
    sys.exit(main())
