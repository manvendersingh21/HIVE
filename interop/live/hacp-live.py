#!/usr/bin/env python3
"""A live, heterogeneous bilateral HACP/2.0 lifecycle: two REAL CLI agents
(codex / agy / claude / opencode — any two, any mix) talking through the file
edge (§12.1), with this script as their adapters.

Division of labor, per the architecture's separation of graphs:
  * the ADAPTERS speak HACP: envelopes, digests, transcripts, schema checks —
    using only the spec, the committed schemas, and peer.py's independent
    canonical core (no HIVE, no Rust reference);
  * the AGENTS do the thinking: side A's CLI authors the contract terms and
    later verifies the artifact; side B's CLI reviews the proposal, then
    executes the frozen contract.

Evidence over signals (§9.4) applies to agents too, so the adapter never
trusts a CLI's word: exit codes are ignored (Phase S finding: a
sandbox-blocked run exited 0), artifacts are opened and hashed, and an
`accept` is only emitted if at least one check the verifier claims is
mechanically re-verified by the adapter to actually hold.
"""

import argparse
import hashlib
import json
import os
import subprocess
import sys
import time
import uuid

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, os.path.join(HERE, "..", "peer-python"))
from peer import (  # noqa: E402
    PROTOCOL,
    canonical,
    check_against_schema,
    digest_of,
    new_message_id,
    now,
    validate_envelope,
)

VOLATILE = ("message_id", "timestamp")

# How each CLI is invoked non-interactively, with a writable workspace.
# codex: --skip-git-repo-check for scratch dirs, workspace-write sandbox.
# agy:   flags before --print (it eats the next token as its prompt).
# claude:-p reads the brief from stdin.
# opencode: prompt as argument.
CLI = {
    "codex":    lambda brief, cwd: (["codex", "exec", "--skip-git-repo-check",
                                     "--sandbox", "workspace-write", "-C", cwd, "-"], brief),
    "agy":      lambda brief, cwd: (["agy", "--add-dir", cwd, "--dangerously-skip-permissions",
                                     "--print", brief], None),
    "claude":   lambda brief, cwd: (["claude", "-p", "--dangerously-skip-permissions"], brief),
    "opencode": lambda brief, cwd: (["opencode", "run", brief], None),
}

CALL_TIMEOUT = 300


def call_cli(name, brief, workdir, log_path):
    argv, stdin_text = CLI[name](brief, workdir)
    started = time.time()
    try:
        proc = subprocess.run(
            argv, input=stdin_text, cwd=workdir, capture_output=True, text=True,
            timeout=CALL_TIMEOUT,
        )
        outcome = f"exit {proc.returncode}"
        stdout, stderr = proc.stdout, proc.stderr
    except subprocess.TimeoutExpired:
        outcome = "timeout"
        stdout, stderr = "", f"{name} exceeded {CALL_TIMEOUT}s"
    with open(log_path, "w", encoding="utf-8") as f:
        f.write(f"$ {' '.join(argv[:2])} ... (brief {len(brief)} chars)\n{outcome}\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}\n")
    return {"cli": name, "outcome": outcome, "seconds": int(time.time() - started)}


RETRY_NOTE = ("\n\nIMPORTANT: your previous reply did not actually create the required "
              "file (this was verified on disk). Create it now using your file-writing "
              "tool. Do not merely print its contents.")


def call_for_file(name, brief_text, workdir, log_base, relpath, tries=2):
    """Call a CLI and require a file to exist afterwards — evidence, not
    signals (§9.4): agents sometimes narrate success without writing."""
    calls = []
    for attempt in range(1, tries + 1):
        suffix = "" if attempt == 1 else f"-retry{attempt}"
        text = brief_text + ("" if attempt == 1 else RETRY_NOTE)
        calls.append(call_cli(name, text, workdir, f"{log_base}{suffix}-log.txt"))
        if os.path.isfile(os.path.join(workdir, relpath)):
            return calls
    return calls


class LiveFailure(Exception):
    pass


class Side:
    """One participant's adapter: inbox/outbox on the file edge + transcript."""

    def __init__(self, label, urn, out_dir, in_dir, schema, transcript_dir):
        self.label = label
        self.urn = urn
        self.out_dir = out_dir
        self.in_dir = in_dir
        self.schema = schema
        self.counter = 0
        self.frames = []
        self.transcript_dir = transcript_dir

    def emit(self, session_id, to, kind, body):
        self.counter += 1
        env = {
            "protocol": PROTOCOL,
            "message_id": new_message_id(),
            "session_id": session_id,
            "from": self.urn,
            "to": to,
            "kind": kind,
            "timestamp": now(),
            "body": body,
        }
        validate_envelope(env)
        check_against_schema(env, self.schema)
        name = f"{self.counter:03d}-{kind}.json"
        with open(os.path.join(self.out_dir, name), "w", encoding="utf-8") as f:
            json.dump(env, f)
        self.frames.append({"dir": f"{self.label}>{'b' if self.label == 'a' else 'a'}", "envelope": env})
        return env

    def receive(self, env):
        validate_envelope(env)
        check_against_schema(env, self.schema)
        if env["to"] != self.urn:
            raise LiveFailure(f"envelope for {env['to']}, not {self.urn}")
        self.frames.append({"dir": f"{'b' if self.label == 'a' else 'a'}>{self.label}", "envelope": env})
        return env

    def read_inbox(self):
        names = sorted(n for n in os.listdir(self.in_dir) if n.endswith(".json"))
        out = []
        for name in names:
            with open(os.path.join(self.in_dir, name), encoding="utf-8") as f:
                out.append(json.load(f))
        return out


def strip_volatile(env):
    return {k: v for k, v in env.items() if k not in VOLATILE}


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--run-dir", required=True)
    ap.add_argument("--supervisor", required=True, choices=sorted(CLI))
    ap.add_argument("--worker", required=True, choices=sorted(CLI))
    ap.add_argument("--schemas", required=True)
    ap.add_argument("--task", default=(
        "produce status.txt containing exactly one line: a status report "
        "confirming the work is done, ending with the word ready"
    ))
    ap.add_argument("--transcripts-out", required=True)
    args = ap.parse_args()

    run = args.run_dir
    for d in ("edge/a-out", "edge/b-out", "workspace", "sup", "wrk", "logs"):
        os.makedirs(os.path.join(run, d), exist_ok=True)
    with open(os.path.join(args.schemas, "envelope.json"), encoding="utf-8") as f:
        schema = json.load(f)

    a = Side("a", "urn:hacp:agent:sup-live-1", os.path.join(run, "edge/a-out"),
             os.path.join(run, "edge/b-out"), schema, run)
    b = Side("b", "urn:hacp:agent:wrk-live-1", os.path.join(run, "edge/b-out"),
             os.path.join(run, "edge/a-out"), schema, run)
    session_id = f"s-{uuid.uuid4().hex[:12]}"
    contract_id = f"c-{uuid.uuid4().hex[:12]}"
    task_id = f"t-{uuid.uuid4().hex[:12]}"
    calls = []
    log = lambda n: os.path.join(run, "logs", n)

    def brief(path, text):
        with open(path, "w", encoding="utf-8") as f:
            f.write(text)
        return text

    # -- §6: handshake -------------------------------------------------------
    a.emit(session_id, b.urn, "session.open", {"prospective": True})
    b.receive(b.read_inbox()[-1])
    b.emit(session_id, a.urn, "session.features",
           {"features": ["delegation", "artifact-digest", "observer-events"]})
    a.receive(a.read_inbox()[-1])
    a.emit(session_id, b.urn, "session.features",
           {"features": ["supervision", "delegation", "artifact-digest", "observer-events"]})
    b.receive(b.read_inbox()[-1])

    # -- side A's agent authors the contract terms ---------------------------
    sup_dir = os.path.join(run, "sup")
    author_brief = brief(log("01-author-brief.txt"), f"""You are the supervisor in a two-agent delegation protocol. Task to delegate:

  {args.task}

Write delegation-terms.json at this absolute path:

  {os.path.join(sup_dir, "delegation-terms.json")}

with EXACTLY this shape:

{{
  "outputs": [{{"name": "<output file name>", "media_type": "text/plain", "one_line": true}}],
  "acceptance": ["<check 1>", "<check 2>", "<check 3>"],
  "budget": {{"max_minutes": 5}}
}}

Rules: acceptance entries must be objective and mechanically checkable by
reading the output file (line counts, required words, non-emptiness). Exactly
one output file. Create the file with your file-writing tool — printing the
JSON as a reply is not writing it. Write the JSON file, nothing else.
""")
    author_calls = call_for_file(args.supervisor, author_brief, sup_dir,
                                 log("01-author"), "delegation-terms.json")
    calls.extend(author_calls)
    try:
        with open(os.path.join(sup_dir, "delegation-terms.json"), encoding="utf-8") as f:
            terms = json.load(f)
        assert terms["outputs"] and terms["acceptance"] and terms["outputs"][0]["name"]
        canonical(terms)
    except Exception as e:
        raise LiveFailure(f"supervisor {args.supervisor} did not author valid terms: {e}")
    output_name = terms["outputs"][0]["name"]

    # -- §7: propose, side B's agent reviews ---------------------------------
    a.emit(session_id, b.urn, "contract.proposed",
           {"contract_id": contract_id, "task_id": task_id, "terms": terms})
    proposal = b.read_inbox()[-1]
    b.receive(proposal)
    wrk_dir = os.path.join(run, "wrk")
    review_brief = brief(log("02-review-brief.txt"), f"""You are the worker in a two-agent delegation protocol. A delegation contract
has been proposed to you:

{json.dumps(proposal["body"], indent=2, sort_keys=True)}

Decide ONLY whether you can complete it exactly as written. If yes, write
accept.json at {os.path.join(wrk_dir, "accept.json")}:
{{"accepted": true}}. If any part is unclear or impossible, write the same
file as {{"accepted": false, "reasons": ["..."]}}. Write the file, nothing else.
""")
    calls.extend(call_for_file(args.worker, review_brief, wrk_dir,
                               log("02-review"), "accept.json"))
    try:
        with open(os.path.join(wrk_dir, "accept.json"), encoding="utf-8") as f:
            decision = json.load(f)
    except Exception as e:
        raise LiveFailure(f"worker {args.worker} did not write accept.json: {e}")
    if decision.get("accepted") is not True:
        raise LiveFailure(f"worker declined the contract: {decision.get('reasons')}")
    b.emit(session_id, a.urn, "contract.accepted", {"contract_id": contract_id, "accepted": True})
    a.receive(a.read_inbox()[-1])
    a.emit(session_id, b.urn, "contract.accepted", {"contract_id": contract_id, "accepted": True})
    b.receive(b.read_inbox()[-1])

    # -- §7.5: freeze; side B recomputes the digest independently ------------
    revision = 1
    frozen_digest = digest_of({"contract_id": contract_id, "revision": revision, "content": terms})
    a.emit(session_id, b.urn, "contract.frozen",
           {"contract_id": contract_id, "revision": revision, "digest": frozen_digest})
    frozen = b.read_inbox()[-1]
    b.receive(frozen)
    if digest_of({"contract_id": contract_id, "revision": revision, "content": terms}) != frozen["body"]["digest"]:
        raise LiveFailure("adapter-side digest disagreement (§7.5)")

    # -- EXECUTE: state, not message. Side B's agent does the work -----------
    work_brief = brief(log("03-work-brief.txt"), f"""The frozen delegation contract below is your work order. Complete its outputs
exactly. Work inside this directory: {wrk_dir}
Write {output_name} at this absolute path: {os.path.join(wrk_dir, output_name)}
Write that one file and nothing else.

{json.dumps(frozen["body"], indent=2, sort_keys=True)}

Contract terms:
{json.dumps(terms, indent=2, sort_keys=True)}
""")
    calls.extend(call_for_file(args.worker, work_brief, wrk_dir,
                               log("03-work"), output_name))
    artifact_path = os.path.join(wrk_dir, output_name)
    if not os.path.isfile(artifact_path):  # evidence, not exit codes (§9.4, Phase S)
        raise LiveFailure(
            f"worker {args.worker} claims completion but {output_name} does not exist"
        )
    raw = open(artifact_path, "rb").read()
    artifact_id = f"urn:hacp:artifact:{uuid.uuid4()}"
    manifest = {
        "artifact_id": artifact_id,
        "media_type": "text/plain",
        "digest": hashlib.sha256(raw).hexdigest(),
        "size": len(raw),
        "producer": b.urn,
        "task_id": task_id,
        "contract_id": contract_id,
        "contract_revision": frozen_digest,
        "derived_from": [],
        "location": os.path.relpath(artifact_path, run),
        "visibility": "participants",
    }
    b.emit(session_id, a.urn, "submission.delivered", {
        "contract_id": contract_id,
        "against_revision": frozen_digest,
        "artifacts": [artifact_id],
        "artifacts_info": [manifest],
        "evidence": [],
        "claim": f"{output_name} written per the frozen terms",
    })
    submission = a.read_inbox()[-1]
    a.receive(submission)

    # -- §9: side A's agent verifies — for real ------------------------------
    criteria = "\n".join(f"  {i}. {c}" for i, c in enumerate(terms["acceptance"], 1))
    verify_brief = brief(log("04-verify-brief.txt"), f"""You are the verifier in a two-agent delegation protocol. A submission arrived.

Artifact: {artifact_path} (claimed sha256 {manifest['digest']}, {manifest['size']} bytes)
The file's contents:
---
{raw.decode('utf-8', 'replace')}
---
Acceptance criteria:
{criteria}

Perform EVERY check for real — read the file again yourself, count lines,
recompute the sha256 (e.g. `shasum -a 256 {artifact_path}`), do not trust the
claims above. Then write verdict.json at {os.path.join(sup_dir, "verdict.json")}
with EXACTLY:

{{"verdict": "accept" | "reject" | "rework",
 "checks": [{{"name": "...", "passed": true|false, "detail": "what you actually did"}}],
 "reasons": ["..."]}}

Write the JSON file, nothing else.
""")
    calls.extend(call_for_file(args.supervisor, verify_brief, sup_dir,
                               log("04-verify"), "verdict.json"))
    try:
        with open(os.path.join(sup_dir, "verdict.json"), encoding="utf-8") as f:
            verdict = json.load(f)
        assert verdict["verdict"] in ("accept", "reject", "rework") and verdict["checks"]
    except Exception as e:
        raise LiveFailure(f"supervisor {args.supervisor} did not write a valid verdict: {e}")

    # The adapter re-verifies mechanically (§9.4): no accept on signals alone.
    actual = hashlib.sha256(raw).hexdigest()
    adapter_checks = {
        "digest": actual == manifest["digest"],
        "size": len(raw) == manifest["size"],
        "exists": os.path.isfile(artifact_path),
        "one_line": raw.count(b"\n") == 1 if terms["outputs"][0].get("one_line") else None,
        "non_empty": len(raw.strip()) > 0,
    }
    corroborated = [c for c in verdict["checks"] if c.get("passed")]
    for check in corroborated:
        name = check.get("name", "").lower()
        if "digest" in name or "sha" in name:
            check["_adapter"] = adapter_checks["digest"]
        elif "line" in name:
            check["_adapter"] = adapter_checks["one_line"]
        elif "size" in name or "bytes" in name:
            check["_adapter"] = adapter_checks["size"]
        elif "empty" in name or "word" in name or "content" in name or "ready" in name:
            check["_adapter"] = adapter_checks["non_empty"]
    backed = [c for c in corroborated if c.get("_adapter") is True]
    if verdict["verdict"] == "accept" and not backed:
        raise LiveFailure(
            "supervisor accepted, but no claimed check survives adapter "
            "re-verification (§9.4: evidence over signals)"
        )

    a.emit(session_id, b.urn, "verification.delivered", {
        "contract_id": contract_id,
        "verdict": verdict["verdict"],
        "artifacts": [artifact_id],
        "checks": [{k: v for k, v in c.items() if k != "_adapter"} for c in verdict["checks"]],
        "reasons": verdict.get("reasons", []),
    })
    verification = b.read_inbox()[-1]
    b.receive(verification)

    # §9.4 mirrored on the worker's side: an accept must be backed.
    if verification["body"]["verdict"] == "accept":
        if not verification["body"]["artifacts"]:
            raise LiveFailure("accept without subject artifacts (§9.4)")
        if not any(c.get("passed") for c in verification["body"]["checks"]):
            raise LiveFailure("accept without a passing check (§9.4)")

    # -- both views of the same exchange must agree --------------------------
    a_view = [{"dir": f["dir"], "envelope": strip_volatile(f["envelope"])} for f in a.frames]
    b_view = [{"dir": f["dir"], "envelope": strip_volatile(f["envelope"])} for f in b.frames]
    if canonical(a_view) != canonical(b_view):
        for i, (fa, fb) in enumerate(zip(a_view, b_view)):
            if canonical(fa) != canonical(fb):
                raise LiveFailure(f"transcript divergence at frame {i}:\nA: {canonical(fa)}\nB: {canonical(fb)}")
        raise LiveFailure(f"transcript length divergence: A={len(a_view)} B={len(b_view)}")

    pair = f"{args.supervisor}x{args.worker}"
    os.makedirs(args.transcripts_out, exist_ok=True)
    tpath = os.path.join(args.transcripts_out, f"live-{pair}.jsonl")
    with open(tpath, "w", encoding="utf-8") as f:
        for frame in a.frames:
            f.write(canonical({"dir": frame["dir"],
                               "envelope": strip_volatile(frame["envelope"])}) + "\n")
    report = {
        "pair": pair,
        "session_id": session_id,
        "contract_id": contract_id,
        "outcome": "settled",
        "verdict": verdict["verdict"],
        "frames": len(a.frames),
        "cli_calls": calls,
        "artifact": manifest,
        "transcript": tpath,
    }
    with open(os.path.join(run, "run-report.json"), "w", encoding="utf-8") as f:
        f.write(canonical(report) + "\n")
    print(f"SETTLED: {pair} — {verdict['verdict']} — {len(a.frames)} frames — {len(backed)} corroborated checks")
    return 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except LiveFailure as e:
        print(f"LIVE FAILURE: {e}", file=sys.stderr)
        sys.exit(1)
