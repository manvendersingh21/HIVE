//! The seven checks (`spec/HACP.md` §11) and the acceptance test.
//!
//! This is the half of the binding that decides whether a run actually happened. Every
//! other module in `collab` moves information around; this one re-derives it. Constraint
//! C2 is the whole design brief: **a worker's claim about its own output is evidence of
//! nothing**, so no field of a [`CompletionReport`] is ever allowed to decide a check.
//! The report supplies a claim; the filesystem, `sha2`, and a subprocess supply the
//! verdict. Where a claim is absent the check says so out loud rather than passing
//! quietly, because an invisible skip reads exactly like a real pass in a verdict.
//!
//! The ordering in §11 is normative and is followed literally: existence, integrity,
//! interface freeze, build probe, symbols, schema, then integration. Check 3 is the one
//! that carries the protocol — it is how a *decided* abstraction stays decided, and how
//! an undeclared edit to a frozen file is told apart from an agreed amendment (§9.2).
//!
//! ## What these checks do not prove
//!
//! Being candid about the ceiling matters more than the checks themselves, because
//! §11.1 pipes a verdict straight into a rework request and a human reads it as fact:
//!
//! * **Check 5 is `grep`.** It finds a literal string somewhere in the artifact tree. It
//!   says nothing about whether that string is a definition, a call, a comment, or a
//!   line in a test fixture — let alone whether it behaves.
//! * **Check 6 enforces a subset of JSON-Schema.** There is no schema crate in this
//!   workspace, so the validator below covers the common keywords and *names every
//!   keyword it did not enforce* in its evidence. A pass means "nothing the validator
//!   understands was violated", never "conforms".
//! * **Check 2 has no meaning for a directory artifact.** HACP v1 defines a file digest;
//!   it defines no canonical digest over a tree. Rather than invent one and fail honest
//!   workers against it, the check records that it verified nothing.
//!
//! Deeper verification is out of scope and §11 forbids a passing verdict from implying
//! it.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use async_trait::async_trait;
use hacp::contract::{ArtifactFormat, ArtifactSpec, InterfaceContract};
use hacp::report::{check, CheckResult, VerificationResult};
use serde_json::Value;
use sha2::{Digest, Sha256};

use super::{Result, VerifyContext, Verifier};

/// How much of a command's output a failed check quotes. Enough to see a compiler error
/// or a failing assertion; small enough that a runaway build log cannot bloat the run
/// record it gets embedded in.
const OUTPUT_TAIL_BYTES: usize = 4 * 1024;

/// Files larger than this are not grepped for symbols. A symbol is an interface name; if
/// it only occurs inside an eight-megabyte generated blob, check 5 finding it would be
/// noise rather than evidence.
const MAX_GREP_FILE_BYTES: u64 = 8 * 1024 * 1024;

/// Directories excluded from the symbol grep. Build output and VCS internals contain
/// copies of the very strings being looked for, so including them turns check 5 from
/// shallow into actively misleading.
const UNSEARCHED_DIRS: &[&str] = &[".git", "target", "node_modules", ".venv"];

/// Where [`HiveVerifier::integrate`] writes the generated acceptance test, relative to
/// the integration root. Kept on disk deliberately: a human asking "what did integration
/// actually assert?" should be able to read the answer and re-run it by hand.
pub const ACCEPTANCE_TEST_PATH: &str = ".hacp/acceptance.sh";

/// Default wall-clock cap on any command this verifier runs.
const DEFAULT_COMMAND_TIMEOUT: Duration = Duration::from_secs(600);

/// Hive's implementation of the §11 checks.
#[derive(Debug, Clone)]
pub struct HiveVerifier {
    command_timeout: Duration,
}

impl HiveVerifier {
    pub fn new() -> Self {
        Self { command_timeout: DEFAULT_COMMAND_TIMEOUT }
    }

    /// Override the wall-clock cap applied to build probes, the integration command, and
    /// the acceptance test.
    pub fn with_timeout(timeout: Duration) -> Self {
        Self { command_timeout: timeout }
    }
}

impl Default for HiveVerifier {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Verifier for HiveVerifier {
    async fn verify(&self, ctx: &VerifyContext<'_>) -> Result<VerificationResult> {
        // The *contract* decides what is checked, not the report. A report cannot shrink
        // its own verification surface by omitting an artifact, and cannot invent one by
        // adding a claim (C2).
        let produced = ctx.contract.produced_by(&ctx.report.agent);
        let mut checks = Vec::new();

        // An empty check list makes `VerificationResult::new` derive `passed = true`, so
        // a role with nothing to produce would otherwise be certified by silence.
        if produced.is_empty() {
            checks.push(CheckResult::pass(
                check::name(check::EXISTENCE, "<none>"),
                format!(
                    "contract {} v{} declares no artifacts produced by {}; \
                     this verdict asserts nothing about the work",
                    ctx.contract.contract_id, ctx.contract.version, ctx.report.agent
                ),
            ));
        }

        for spec in &produced {
            let root = artifact_root(ctx.workspace, spec);

            checks.push(check_existence(spec, &root));
            checks.push(check_integrity(ctx, spec, &root));
            checks.push(check_interface_frozen(ctx, spec, &root));
            checks.push(self.check_build_probe(spec, ctx.workspace).await);
            checks.push(check_symbols(spec, &root));
            checks.push(check_schema(spec, &root));
        }

        // A claim naming an artifact the frozen contract does not declare is worth
        // failing on: it means the worker and the arbiter disagree about what was
        // commissioned, which §9.2 has a door for (`contract.change.requested`) and this
        // is not it.
        for claim in &ctx.report.artifacts {
            if ctx.contract.artifact(&claim.artifact_id).is_none() {
                checks.push(CheckResult::fail(
                    check::name(check::EXISTENCE, &claim.artifact_id),
                    format!(
                        "report {} claims artifact {:?} at {:?}, which contract {} v{} \
                         does not declare; an artifact that is not in the frozen contract \
                         cannot be verified against it, and adding one requires \
                         contract.change.requested (§9.2)",
                        ctx.report.report_id,
                        claim.artifact_id,
                        claim.path,
                        ctx.contract.contract_id,
                        ctx.contract.version,
                    ),
                ));
            }
        }

        Ok(VerificationResult::new(
            ctx.report.agent.clone(),
            ctx.report.report_id.clone(),
            checks,
        ))
    }

    async fn integrate(&self, contract: &InterfaceContract, integration_root: &Path) -> Result<CheckResult> {
        let name = check::name(check::INTEGRATION, &contract.contract_id);
        let mut evidence = String::new();

        // §11.7: the merged result is exercised by the contract's own command *and* by
        // the arbiter-authored acceptance test. Running only the former would make
        // integration mean "the command the workers were told about passed", which is
        // one step away from self-assessment.
        if let Some(integration) = &contract.integration {
            let outcome = run_command(&integration.command, integration_root, self.command_timeout).await?;
            evidence.push_str(&describe_command(
                "integration.command",
                &integration.command,
                integration_root,
                &outcome,
            ));
            if !outcome.succeeded() {
                evidence.push_str(
                    "\nacceptance test: not run — the integration command must pass first, \
                     or its failures are reported twice",
                );
                return Ok(CheckResult::fail(name, evidence));
            }
        } else {
            evidence.push_str("integration.command: none declared by the contract\n");
        }

        let script = self.acceptance_test(contract)?;
        let script_path = integration_root.join(ACCEPTANCE_TEST_PATH);
        if let Some(parent) = script_path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        tokio::fs::write(&script_path, script.as_bytes()).await?;

        let command = format!("sh {}", shell_quote(&script_path.to_string_lossy()));
        let outcome = run_command(&command, integration_root, self.command_timeout).await?;
        evidence.push('\n');
        evidence.push_str(&describe_command("acceptance test", &command, integration_root, &outcome));
        evidence.push_str(&format!("\nacceptance test retained at {}", script_path.display()));

        if outcome.succeeded() {
            Ok(CheckResult::pass(name, evidence))
        } else {
            Ok(CheckResult::fail(name, evidence))
        }
    }

    fn acceptance_test(&self, contract: &InterfaceContract) -> Result<String> {
        let mut s = String::new();
        s.push_str("#!/bin/sh\n");
        s.push_str("# HACP acceptance test — generated by the arbiter from the frozen contract.\n");
        s.push_str("#\n");
        s.push_str(&format!(
            "# contract : {} (version {})\n# goal     : {}\n",
            contract.contract_id,
            contract.version,
            one_line(&contract.goal)
        ));
        s.push_str("#\n");
        s.push_str("# Every assertion below comes from an `examples` entry that the workers agreed\n");
        s.push_str("# to before any of them started. This script is what makes integration success\n");
        s.push_str("# mean \"the frozen contract executed\" rather than \"an agent said it worked\".\n");
        s.push_str("#\n");
        s.push_str("# How an example is executed: the artifact's `check.command` is run from the\n");
        s.push_str("# repository root with the example's input on stdin, and the example's output\n");
        s.push_str("# must appear in the command's combined output. HACP v1 does not say whether\n");
        s.push_str("# the match is exact, so containment is used — and stated here rather than\n");
        s.push_str("# left for someone to infer from a failure.\n");
        s.push_str("#\n");
        s.push_str("# Do not edit: regenerated on every integration.\n\n");
        s.push_str("set -u\n\n");
        s.push_str("total=0\nfailures=0\n\n");
        s.push_str("assert_example() {\n");
        s.push_str("  label=$1; command=$2; input=$3; expected=$4\n");
        s.push_str("  total=$((total + 1))\n");
        s.push_str("  printf '\\n--- %s ---\\n' \"$label\"\n");
        s.push_str("  printf 'command  : %s\\n' \"$command\"\n");
        s.push_str("  printf 'input    : %s\\n' \"$input\"\n");
        s.push_str("  printf 'expected : %s\\n' \"$expected\"\n");
        s.push_str("  actual=$(printf '%s' \"$input\" | sh -c \"$command\" 2>&1)\n");
        s.push_str("  status=$?\n");
        s.push_str("  case \"$actual\" in\n");
        s.push_str("    *\"$expected\"*)\n");
        s.push_str("      printf 'result   : PASS\\n'\n");
        s.push_str("      ;;\n");
        s.push_str("    *)\n");
        s.push_str("      failures=$((failures + 1))\n");
        s.push_str("      printf 'result   : FAIL (exit %s)\\n' \"$status\"\n");
        s.push_str("      printf 'actual   : %s\\n' \"$actual\"\n");
        s.push_str("      ;;\n");
        s.push_str("  esac\n");
        s.push_str("}\n\n");

        let mut assertions = 0usize;
        for spec in &contract.artifacts {
            if spec.examples.is_empty() {
                continue;
            }
            s.push_str(&format!("# artifact {} at {}\n", spec.artifact_id, spec.path));
            let Some(check) = &spec.check else {
                // Not silently dropped: an example with nothing to run it is a hole in
                // the contract, and the hole belongs in the artifact a human reads.
                s.push_str(&format!(
                    "# SKIPPED: {} example(s) declared, but the artifact has no check.command\n\
                     #          to run them against, so nothing here is asserted.\n\n",
                    spec.examples.len()
                ));
                continue;
            };
            for (i, example) in spec.examples.iter().enumerate() {
                // Contract validation (§9) guarantees strings; an unvalidated contract
                // is reported rather than coerced.
                let input = example.input.as_str().ok_or_else(|| {
                    anyhow::anyhow!(
                        "artifact {}: example {} input is not a JSON string; \
                         the contract was not validated (§9)",
                        spec.artifact_id,
                        i + 1
                    )
                })?;
                let output = example.output.as_str().ok_or_else(|| {
                    anyhow::anyhow!(
                        "artifact {}: example {} output is not a JSON string; \
                         the contract was not validated (§9)",
                        spec.artifact_id,
                        i + 1
                    )
                })?;
                s.push_str(&format!(
                    "assert_example {} {} {} {}\n",
                    shell_quote(&format!("{} example {}", spec.artifact_id, i + 1)),
                    shell_quote(&check.command),
                    shell_quote(input),
                    shell_quote(output),
                ));
                assertions += 1;
            }
            s.push('\n');
        }

        if assertions == 0 {
            s.push_str("# The contract declares no runnable examples. This script therefore asserts\n");
            s.push_str("# nothing, and a passing integration says nothing about behaviour.\n");
            s.push_str("printf '\\nno runnable examples in this contract\\n'\n");
        }

        s.push_str("\nprintf '\\n%s of %s example(s) passed\\n' \"$((total - failures))\" \"$total\"\n");
        s.push_str("[ \"$failures\" -eq 0 ] || exit 1\n");
        Ok(s)
    }
}

// ---------------------------------------------------------------------------
// Checks 1-6
// ---------------------------------------------------------------------------

/// Where an artifact lives. `path` is relative to the shared repository root (§14) and a
/// worker realizes it inside its own workspace, so the workspace is the repository root
/// from a verifier's point of view.
fn artifact_root(workspace: &Path, spec: &ArtifactSpec) -> PathBuf {
    workspace.join(&spec.path)
}

/// Check 1 — the artifact exists at its declared path.
fn check_existence(spec: &ArtifactSpec, root: &Path) -> CheckResult {
    let name = check::name(check::EXISTENCE, &spec.artifact_id);
    match std::fs::symlink_metadata(root) {
        Ok(meta) => {
            let kind = if meta.is_dir() {
                "directory"
            } else if meta.is_file() {
                "file"
            } else {
                "symlink"
            };
            CheckResult::pass(name, format!("{} exists at {} ({})", spec.artifact_id, root.display(), kind))
        }
        Err(e) => CheckResult::fail(
            name,
            format!(
                "{} declares path {:?}, which does not resolve under the workspace: \
                 {} ({e})",
                spec.artifact_id,
                spec.path,
                root.display()
            ),
        ),
    }
}

/// Check 2 — sha256 matches the report's claim, where claimed.
fn check_integrity(ctx: &VerifyContext<'_>, spec: &ArtifactSpec, root: &Path) -> CheckResult {
    let name = check::name(check::INTEGRITY, &spec.artifact_id);
    let claim = ctx
        .report
        .artifacts
        .iter()
        .find(|a| a.artifact_id == spec.artifact_id)
        .and_then(|a| a.sha256.as_deref());

    let Some(claim) = claim else {
        // §11.2 says "where claimed". A skip is still recorded, so the verdict never
        // reads as though a digest was compared when none was offered.
        return CheckResult::pass(
            name,
            format!(
                "report {} claims no sha256 for {}; nothing to compare (§11.2 checks \
                 integrity only where claimed)",
                ctx.report.report_id, spec.artifact_id
            ),
        );
    };

    match std::fs::symlink_metadata(root) {
        Err(e) => CheckResult::fail(
            name,
            format!("cannot hash {}: {e}\n  claimed: {claim}", root.display()),
        ),
        Ok(meta) if meta.is_dir() => CheckResult::pass(
            name,
            format!(
                "{} is a directory at {}. HACP v1 defines sha256 for a file artifact and \
                 no canonical digest over a tree, so the claim {claim} was NOT re-derived. \
                 This check verified nothing; the tree's frozen surface is covered by \
                 interface-frozen:{} instead.",
                spec.artifact_id,
                root.display(),
                spec.artifact_id
            ),
        ),
        Ok(_) => match std::fs::read(root) {
            Err(e) => CheckResult::fail(
                name,
                format!("cannot read {}: {e}\n  claimed: {claim}", root.display()),
            ),
            Ok(bytes) => {
                let actual = format!("sha256:{:x}", Sha256::digest(&bytes));
                if digests_equal(claim, &actual) {
                    CheckResult::pass(
                        name,
                        format!("{}\n  claimed: {claim}\n  actual : {actual}", root.display()),
                    )
                } else {
                    CheckResult::fail(
                        name,
                        format!(
                            "sha256 mismatch for {} at {}\n  claimed: {claim}\n  actual : {actual}\n\
                             The bytes on disk are not the bytes the report describes.",
                            spec.artifact_id,
                            root.display()
                        ),
                    )
                }
            }
        },
    }
}

/// Check 3 — the recomputed interface digest equals the digest in force.
///
/// The heart of the protocol. `ctx.frozen_digests` holds what was agreed: from
/// `contract.frozen`, or from the most recent `contract.amended` (§9.2). A mismatch means
/// a frozen file changed without the door in §9.2 being opened, which is precisely the
/// failure the freeze exists to catch — so the evidence quotes both digests and names the
/// files, because §11.1 hands this text to the worker as its rework request.
fn check_interface_frozen(ctx: &VerifyContext<'_>, spec: &ArtifactSpec, root: &Path) -> CheckResult {
    let name = check::name(check::INTERFACE_FROZEN, &spec.artifact_id);
    let expected = ctx.frozen_digests.get(&spec.artifact_id);

    if spec.interface_files.is_empty() && expected.is_none() {
        return CheckResult::pass(
            name,
            format!(
                "{} declares no interface_files and the freeze recorded no digest for it; \
                 it exposes no frozen interface, so nothing about it can drift",
                spec.artifact_id
            ),
        );
    }

    // Resolve interface_files "relative to the artifact" (§9). For a directory artifact
    // that is the directory; for a single-file artifact the only sensible base is the
    // directory containing it, since a path cannot be joined onto a file.
    let base = match std::fs::symlink_metadata(root) {
        Ok(meta) if meta.is_file() => root.parent().unwrap_or(root).to_path_buf(),
        _ => root.to_path_buf(),
    };

    let mut missing = Vec::new();
    let actual = spec.interface_digest(|rel| -> std::result::Result<Vec<u8>, std::io::Error> {
        let path = base.join(rel);
        match std::fs::read(&path) {
            Ok(bytes) => Ok(bytes),
            Err(e) => {
                missing.push(format!("{} ({e})", path.display()));
                Err(e)
            }
        }
    });

    let actual = match actual {
        Ok(d) => d,
        Err(_) => {
            return CheckResult::fail(
                name,
                format!(
                    "cannot recompute the interface digest for {}: a frozen interface file \
                     is unreadable\n  base    : {}\n  missing : {}\n  expected: {}\n\
                     A frozen file that has been deleted or moved is an undeclared \
                     interface change (§9.2).",
                    spec.artifact_id,
                    base.display(),
                    missing.join(", "),
                    expected.map(String::as_str).unwrap_or("<none recorded>"),
                ),
            )
        }
    };

    let Some(expected) = expected else {
        return CheckResult::fail(
            name,
            format!(
                "no frozen digest recorded for {}, but it declares interface_files {:?}\n  \
                 recomputed: {actual}\n\
                 The freeze is incomplete: there is nothing to compare against, so an \
                 undeclared change to these files could not be detected.",
                spec.artifact_id, spec.interface_files
            ),
        );
    };

    if digests_equal(expected, &actual) {
        CheckResult::pass(
            name,
            format!(
                "interface_files {:?} under {} are unchanged\n  frozen    : {expected}\n  \
                 recomputed: {actual}",
                spec.interface_files,
                base.display()
            ),
        )
    } else {
        CheckResult::fail(
            name,
            format!(
                "FROZEN INTERFACE CHANGED for {}\n  files     : {:?}\n  base      : {}\n  \
                 frozen    : {expected}\n  recomputed: {actual}\n\
                 A frozen interface file was edited without an accepted amendment. §9.2 \
                 requires contract.change.requested BEFORE the file is changed; revert \
                 these files to the frozen contents, or request the change and wait for \
                 contract.amended.",
                spec.artifact_id,
                spec.interface_files,
                base.display()
            ),
        )
    }
}

impl HiveVerifier {
    /// Check 4 — the artifact's `check.command` exits 0, run from the repository root.
    async fn check_build_probe(&self, spec: &ArtifactSpec, repo_root: &Path) -> CheckResult {
        let name = check::name(check::BUILD_PROBE, &spec.artifact_id);
        let Some(probe) = &spec.check else {
            return CheckResult::pass(
                name,
                format!("{} declares no check.command; no build probe was run", spec.artifact_id),
            );
        };
        match run_command(&probe.command, repo_root, self.command_timeout).await {
            Ok(outcome) => {
                let evidence = describe_command("check.command", &probe.command, repo_root, &outcome);
                if outcome.succeeded() {
                    CheckResult::pass(name, evidence)
                } else {
                    CheckResult::fail(name, evidence)
                }
            }
            Err(e) => CheckResult::fail(
                name,
                format!(
                    "could not run check.command for {}\n  command: {}\n  cwd    : {}\n  error  : {e}",
                    spec.artifact_id,
                    probe.command,
                    repo_root.display()
                ),
            ),
        }
    }
}

/// Check 5 — each `symbols` entry occurs literally somewhere in the artifact tree.
///
/// Shallow by design and labelled as such in its own evidence (§11). Finding
/// `submit_job` proves a byte sequence is present in a file; it does not prove there is a
/// function, that it is exported, that it takes the arguments a consumer expects, or that
/// it does anything. A passing symbol check is the weakest evidence in the verdict and
/// must never be read as more.
fn check_symbols(spec: &ArtifactSpec, root: &Path) -> CheckResult {
    let name = check::name(check::SYMBOLS, &spec.artifact_id);
    if spec.symbols.is_empty() {
        return CheckResult::pass(
            name,
            format!("{} declares no symbols; nothing was grepped for", spec.artifact_id),
        );
    }

    let mut files = Vec::new();
    let mut walk_errors = Vec::new();
    collect_files(root, &mut files, &mut walk_errors);

    let caveat = format!(
        "\nThis is a literal grep over {} file(s) under {} (excluding {:?}), not proof of \
         semantics: a hit may be a definition, a call, a comment, or a test fixture, and \
         says nothing about behaviour or signature.",
        files.len(),
        root.display(),
        UNSEARCHED_DIRS
    );

    let mut found = Vec::new();
    let mut missing = Vec::new();
    for symbol in &spec.symbols {
        match first_hit(&files, symbol) {
            Some((path, line)) => found.push(format!("  {symbol}  found at {}:{line}", path.display())),
            None => missing.push(symbol.clone()),
        }
    }

    let mut evidence = String::new();
    if !found.is_empty() {
        evidence.push_str(&found.join("\n"));
        evidence.push('\n');
    }
    if !walk_errors.is_empty() {
        evidence.push_str(&format!("  unreadable while walking: {}\n", walk_errors.join(", ")));
    }

    if missing.is_empty() {
        evidence.push_str(&format!("all {} declared symbol(s) present.{caveat}", spec.symbols.len()));
        CheckResult::pass(name, evidence)
    } else {
        evidence.push_str(&format!(
            "MISSING from the artifact tree: {missing:?}\n  \
             searched: {}\n  declared: {:?}{caveat}",
            root.display(),
            spec.symbols
        ));
        CheckResult::fail(name, evidence)
    }
}

/// Check 6 — `format: "json"` artifacts validate against their JSON-Schema.
fn check_schema(spec: &ArtifactSpec, root: &Path) -> CheckResult {
    let name = check::name(check::SCHEMA, &spec.artifact_id);
    if spec.format != ArtifactFormat::Json {
        return CheckResult::pass(
            name,
            format!(
                "{} has format {:?}; §9 permits a schema only on format \"json\", so there \
                 is nothing to validate",
                spec.artifact_id, spec.format
            ),
        );
    }
    let Some(schema) = &spec.schema else {
        // Contract validation rejects this, so reaching it means an unvalidated contract.
        return CheckResult::fail(
            name,
            format!(
                "{} declares format \"json\" but carries no schema; §9 requires one, so \
                 this contract was never validated",
                spec.artifact_id
            ),
        );
    };

    let bytes = match std::fs::read(root) {
        Ok(b) => b,
        Err(e) => {
            return CheckResult::fail(
                name,
                format!("cannot read the JSON artifact {}: {e}", root.display()),
            )
        }
    };
    let instance: Value = match serde_json::from_slice(&bytes) {
        Ok(v) => v,
        Err(e) => {
            return CheckResult::fail(
                name,
                format!(
                    "{} is not valid JSON\n  path : {}\n  error: {e}",
                    spec.artifact_id,
                    root.display()
                ),
            )
        }
    };

    let mut errors = Vec::new();
    let mut unenforced = BTreeSet::new();
    validate_against(&instance, schema, "$", &mut errors, &mut unenforced);

    let caveat = if unenforced.is_empty() {
        String::from(
            "\nEvery keyword in this schema is enforced by the built-in validator.",
        )
    } else {
        format!(
            "\nNOT ENFORCED by the built-in validator: {:?}. This workspace has no \
             JSON-Schema crate, so a pass means \"nothing the validator understands was \
             violated\", not \"conforms to the schema\".",
            unenforced
        )
    };

    if errors.is_empty() {
        CheckResult::pass(
            name,
            format!("{} validated against its schema.{caveat}", root.display()),
        )
    } else {
        CheckResult::fail(
            name,
            format!(
                "{} does not satisfy its schema:\n{}{caveat}",
                root.display(),
                errors.iter().map(|e| format!("  {e}")).collect::<Vec<_>>().join("\n")
            ),
        )
    }
}

// ---------------------------------------------------------------------------
// Running commands
// ---------------------------------------------------------------------------

/// The result of one external command.
struct CommandOutcome {
    status: Option<i32>,
    timed_out: bool,
    output: String,
}

impl CommandOutcome {
    fn succeeded(&self) -> bool {
        !self.timed_out && self.status == Some(0)
    }
}

/// Run `command` through `sh -c` in `cwd`, capped at `timeout`.
///
/// `kill_on_drop` matters: a build probe that hangs would otherwise outlive the timeout
/// and hold the workspace. This is not the worker-session rule ("pause, never kill") —
/// that protects state a human is about to inspect; a wedged verification subprocess has
/// no such state and must not be allowed to stall the run.
async fn run_command(command: &str, cwd: &Path, timeout: Duration) -> Result<CommandOutcome> {
    let mut cmd = tokio::process::Command::new("sh");
    cmd.arg("-c").arg(command).current_dir(cwd).kill_on_drop(true);

    match tokio::time::timeout(timeout, cmd.output()).await {
        Err(_) => Ok(CommandOutcome {
            status: None,
            timed_out: true,
            output: String::new(),
        }),
        Ok(Err(e)) => Err(anyhow::anyhow!("spawning `{command}` in {} failed: {e}", cwd.display())),
        Ok(Ok(out)) => {
            let mut combined = String::from_utf8_lossy(&out.stdout).into_owned();
            let stderr = String::from_utf8_lossy(&out.stderr);
            if !stderr.is_empty() {
                if !combined.is_empty() && !combined.ends_with('\n') {
                    combined.push('\n');
                }
                combined.push_str(&stderr);
            }
            Ok(CommandOutcome {
                status: out.status.code(),
                timed_out: false,
                output: combined,
            })
        }
    }
}

/// Render a command run as evidence: what ran, where, how it ended, and the tail of what
/// it said. A check that reports only "failed" is unactionable (§11).
fn describe_command(label: &str, command: &str, cwd: &Path, outcome: &CommandOutcome) -> String {
    let status = if outcome.timed_out {
        "TIMED OUT (killed)".to_string()
    } else {
        match outcome.status {
            Some(0) => "exit 0".to_string(),
            Some(c) => format!("exit {c} (expected 0)"),
            None => "terminated by signal".to_string(),
        }
    };
    let tail = tail_of(&outcome.output, OUTPUT_TAIL_BYTES);
    format!(
        "{label}: {command}\n  cwd   : {}\n  status: {status}\n  output:\n{}",
        cwd.display(),
        if tail.is_empty() {
            "    <no output>".to_string()
        } else {
            tail.lines().map(|l| format!("    {l}")).collect::<Vec<_>>().join("\n")
        }
    )
}

/// The last `limit` bytes of `s`, cut on a char boundary.
fn tail_of(s: &str, limit: usize) -> String {
    if s.len() <= limit {
        return s.trim_end().to_string();
    }
    let mut start = s.len() - limit;
    while start < s.len() && !s.is_char_boundary(start) {
        start += 1;
    }
    format!("[... {} earlier bytes omitted ...]\n{}", start, s[start..].trim_end())
}

// ---------------------------------------------------------------------------
// Filesystem helpers
// ---------------------------------------------------------------------------

/// Every regular file under `root` (or `root` itself, if it is one), excluding build and
/// VCS directories. Symlinks are not followed: a link out of the workspace is not part of
/// the artifact, and a link cycle would not terminate.
fn collect_files(root: &Path, out: &mut Vec<PathBuf>, errors: &mut Vec<String>) {
    let meta = match std::fs::symlink_metadata(root) {
        Ok(m) => m,
        Err(e) => {
            errors.push(format!("{} ({e})", root.display()));
            return;
        }
    };
    if meta.is_file() {
        out.push(root.to_path_buf());
        return;
    }
    if !meta.is_dir() {
        return;
    }
    let entries = match std::fs::read_dir(root) {
        Ok(e) => e,
        Err(e) => {
            errors.push(format!("{} ({e})", root.display()));
            return;
        }
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if UNSEARCHED_DIRS.contains(&name.as_ref()) {
            continue;
        }
        collect_files(&path, out, errors);
    }
    out.sort();
}

/// The first `path:line` where `needle` occurs literally, scanning files in sorted order
/// so the evidence is stable across runs.
fn first_hit<'a>(files: &'a [PathBuf], needle: &str) -> Option<(&'a Path, usize)> {
    for path in files {
        let Ok(meta) = std::fs::metadata(path) else { continue };
        if meta.len() > MAX_GREP_FILE_BYTES {
            continue;
        }
        // Lossy so a symbol inside an otherwise-binary file is still found; the line
        // number is then approximate for that file, which the caveat already covers.
        let Ok(bytes) = std::fs::read(path) else { continue };
        let text = String::from_utf8_lossy(&bytes);
        if let Some(idx) = text.find(needle) {
            let line = text[..idx].matches('\n').count() + 1;
            return Some((path.as_path(), line));
        }
    }
    None
}

/// Compare two digests tolerating the `sha256:` prefix and case, since a worker may write
/// either form. The *values* are still compared byte for byte.
fn digests_equal(a: &str, b: &str) -> bool {
    fn norm(s: &str) -> String {
        s.trim().trim_start_matches("sha256:").to_ascii_lowercase()
    }
    norm(a) == norm(b)
}

/// Wrap `s` in single quotes for POSIX `sh`, escaping any single quote it contains.
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Collapse a string to one line, for use in a generated comment.
fn one_line(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

// ---------------------------------------------------------------------------
// JSON-Schema subset (check 6)
// ---------------------------------------------------------------------------

/// Keywords that carry no constraint, so ignoring them is not under-enforcement.
const ANNOTATION_KEYWORDS: &[&str] = &[
    "$schema", "$id", "$comment", "title", "description", "default", "examples", "deprecated",
    "readOnly", "writeOnly", "definitions", "$defs",
];

/// Validate `instance` against `schema`, appending human-readable failures to `errors`
/// and the name of every keyword this validator does *not* implement to `unenforced`.
///
/// Tracking the unenforced set is the point. Without it a pass would silently mean
/// "your schema used `$ref`, which we ignored", and a verdict that overstates what it
/// checked is worse than a missing check (§11).
fn validate_against(
    instance: &Value,
    schema: &Value,
    path: &str,
    errors: &mut Vec<String>,
    unenforced: &mut BTreeSet<String>,
) {
    let obj = match schema {
        // A boolean schema: `true` accepts anything, `false` rejects everything.
        Value::Bool(true) => return,
        Value::Bool(false) => {
            errors.push(format!("{path}: schema is `false`, which accepts nothing"));
            return;
        }
        Value::Object(o) => o,
        other => {
            errors.push(format!("{path}: schema is not an object or boolean (got {other})"));
            return;
        }
    };

    for key in obj.keys() {
        if !ANNOTATION_KEYWORDS.contains(&key.as_str()) && !is_supported_keyword(key) {
            unenforced.insert(key.clone());
        }
    }

    if let Some(t) = obj.get("type") {
        let types: Vec<&str> = match t {
            Value::String(s) => vec![s.as_str()],
            Value::Array(a) => a.iter().filter_map(|v| v.as_str()).collect(),
            _ => Vec::new(),
        };
        if !types.is_empty() && !types.iter().any(|t| json_type_matches(instance, t)) {
            errors.push(format!(
                "{path}: expected type {:?}, found {}",
                types,
                json_type_name(instance)
            ));
        }
    }

    if let Some(Value::Array(allowed)) = obj.get("enum") {
        if !allowed.contains(instance) {
            errors.push(format!("{path}: value {instance} is not one of {allowed:?}"));
        }
    }
    if let Some(expected) = obj.get("const") {
        if instance != expected {
            errors.push(format!("{path}: expected const {expected}, found {instance}"));
        }
    }

    match instance {
        Value::Object(map) => {
            if let Some(Value::Array(required)) = obj.get("required") {
                for r in required.iter().filter_map(|v| v.as_str()) {
                    if !map.contains_key(r) {
                        errors.push(format!("{path}: missing required property {r:?}"));
                    }
                }
            }
            let properties = obj.get("properties").and_then(Value::as_object);
            if let Some(props) = properties {
                for (key, sub) in props {
                    if let Some(child) = map.get(key) {
                        validate_against(child, sub, &format!("{path}.{key}"), errors, unenforced);
                    }
                }
            }
            if let Some(additional) = obj.get("additionalProperties") {
                for (key, child) in map {
                    if properties.is_some_and(|p| p.contains_key(key)) {
                        continue;
                    }
                    match additional {
                        Value::Bool(false) => {
                            errors.push(format!("{path}: unexpected property {key:?}"))
                        }
                        Value::Bool(true) => {}
                        sub => validate_against(
                            child,
                            sub,
                            &format!("{path}.{key}"),
                            errors,
                            unenforced,
                        ),
                    }
                }
            }
        }
        Value::Array(items) => {
            if let Some(sub) = obj.get("items") {
                for (i, child) in items.iter().enumerate() {
                    validate_against(child, sub, &format!("{path}[{i}]"), errors, unenforced);
                }
            }
            if let Some(min) = obj.get("minItems").and_then(Value::as_u64) {
                if (items.len() as u64) < min {
                    errors.push(format!("{path}: {} items, minItems is {min}", items.len()));
                }
            }
            if let Some(max) = obj.get("maxItems").and_then(Value::as_u64) {
                if (items.len() as u64) > max {
                    errors.push(format!("{path}: {} items, maxItems is {max}", items.len()));
                }
            }
            if obj.get("uniqueItems") == Some(&Value::Bool(true)) {
                for (i, a) in items.iter().enumerate() {
                    if items[..i].contains(a) {
                        errors.push(format!("{path}[{i}]: duplicate item {a}, uniqueItems is true"));
                    }
                }
            }
        }
        Value::String(s) => {
            if let Some(min) = obj.get("minLength").and_then(Value::as_u64) {
                if (s.chars().count() as u64) < min {
                    errors.push(format!("{path}: length {} is below minLength {min}", s.chars().count()));
                }
            }
            if let Some(max) = obj.get("maxLength").and_then(Value::as_u64) {
                if (s.chars().count() as u64) > max {
                    errors.push(format!("{path}: length {} exceeds maxLength {max}", s.chars().count()));
                }
            }
            if let Some(pattern) = obj.get("pattern").and_then(Value::as_str) {
                // JSON-Schema patterns are ECMA regexes; Rust's `regex` rejects some of
                // them (lookaround especially). An uncompilable pattern is declared
                // unenforced rather than reported as a violation of the instance.
                match regex::Regex::new(pattern) {
                    Ok(re) if !re.is_match(s) => {
                        errors.push(format!("{path}: {s:?} does not match pattern {pattern:?}"))
                    }
                    Ok(_) => {}
                    Err(_) => {
                        unenforced.insert(format!("pattern({pattern})"));
                    }
                }
            }
        }
        Value::Number(_) => {
            let n = instance.as_f64();
            if let (Some(n), Some(min)) = (n, obj.get("minimum").and_then(Value::as_f64)) {
                if n < min {
                    errors.push(format!("{path}: {n} is below minimum {min}"));
                }
            }
            if let (Some(n), Some(max)) = (n, obj.get("maximum").and_then(Value::as_f64)) {
                if n > max {
                    errors.push(format!("{path}: {n} exceeds maximum {max}"));
                }
            }
            if let (Some(n), Some(min)) = (n, obj.get("exclusiveMinimum").and_then(Value::as_f64)) {
                if n <= min {
                    errors.push(format!("{path}: {n} is not above exclusiveMinimum {min}"));
                }
            }
            if let (Some(n), Some(max)) = (n, obj.get("exclusiveMaximum").and_then(Value::as_f64)) {
                if n >= max {
                    errors.push(format!("{path}: {n} is not below exclusiveMaximum {max}"));
                }
            }
        }
        _ => {}
    }

    if let Some(Value::Array(subs)) = obj.get("allOf") {
        for sub in subs {
            validate_against(instance, sub, path, errors, unenforced);
        }
    }
    if let Some(Value::Array(subs)) = obj.get("anyOf") {
        if !subs.iter().any(|s| subschema_matches(instance, s, unenforced)) {
            errors.push(format!("{path}: value matches none of the {} anyOf branches", subs.len()));
        }
    }
    if let Some(Value::Array(subs)) = obj.get("oneOf") {
        let hits = subs.iter().filter(|s| subschema_matches(instance, s, unenforced)).count();
        if hits != 1 {
            errors.push(format!("{path}: matches {hits} oneOf branches, expected exactly 1"));
        }
    }
    if let Some(sub) = obj.get("not") {
        if subschema_matches(instance, sub, unenforced) {
            errors.push(format!("{path}: value matches a `not` schema"));
        }
    }
}

/// Whether a branch of `anyOf` / `oneOf` / `not` accepts the instance. Its errors are
/// discarded — only the verdict matters — but its unenforced keywords are kept, so a
/// branch using something unimplemented still shows up in the caveat.
fn subschema_matches(instance: &Value, schema: &Value, unenforced: &mut BTreeSet<String>) -> bool {
    let mut errors = Vec::new();
    validate_against(instance, schema, "$", &mut errors, unenforced);
    errors.is_empty()
}

fn is_supported_keyword(key: &str) -> bool {
    matches!(
        key,
        "type"
            | "enum"
            | "const"
            | "required"
            | "properties"
            | "additionalProperties"
            | "items"
            | "minItems"
            | "maxItems"
            | "uniqueItems"
            | "minLength"
            | "maxLength"
            | "pattern"
            | "minimum"
            | "maximum"
            | "exclusiveMinimum"
            | "exclusiveMaximum"
            | "allOf"
            | "anyOf"
            | "oneOf"
            | "not"
    )
}

fn json_type_matches(v: &Value, t: &str) -> bool {
    match t {
        "object" => v.is_object(),
        "array" => v.is_array(),
        "string" => v.is_string(),
        "boolean" => v.is_boolean(),
        "null" => v.is_null(),
        "number" => v.is_number(),
        "integer" => v.as_i64().is_some() || v.as_u64().is_some() || v.as_f64().is_some_and(|f| f.fract() == 0.0),
        _ => false,
    }
}

fn json_type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use hacp::contract::{ContractCheck, ExamplePair, IntegrationSpec};
    use hacp::report::{CompletionReport, ContractStatus, Outcome, ReportArtifact, ReportSource};
    use std::collections::BTreeMap;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// A self-cleaning temp directory. The workspace has no `tempfile` dependency and
    /// this parcel may not add one, so this is the minimum that does the job.
    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn new(tag: &str) -> Self {
            static N: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "hive-verify-{}-{tag}-{}",
                std::process::id(),
                N.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&path).expect("create temp dir");
            Self { path }
        }

        fn write(&self, rel: &str, contents: &str) -> PathBuf {
            let path = self.path.join(rel);
            std::fs::create_dir_all(path.parent().expect("has parent")).expect("create parent");
            std::fs::write(&path, contents).expect("write fixture");
            path
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    const AGENT: &str = "urn:hacp:agent:a-0001";

    fn artifact() -> ArtifactSpec {
        ArtifactSpec {
            artifact_id: "job-store".into(),
            produced_by: AGENT.into(),
            path: "src/store".into(),
            format: ArtifactFormat::File,
            schema: None,
            interface_files: vec!["api.md".into()],
            symbols: vec!["submit_job".into()],
            examples: Vec::new(),
            check: None,
        }
    }

    fn contract(artifacts: Vec<ArtifactSpec>) -> InterfaceContract {
        InterfaceContract {
            contract_id: "c-test".into(),
            version: 1,
            goal: "verify the verifier".into(),
            artifacts,
            dependencies: Vec::new(),
            integration: None,
            workspace_rules: Vec::new(),
        }
    }

    fn report(artifacts: Vec<ReportArtifact>) -> CompletionReport {
        CompletionReport {
            report_id: "r-test".into(),
            agent: AGENT.into(),
            outcome: Outcome::Success,
            summary: "did the thing".into(),
            artifacts,
            diffstat: None,
            tests: None,
            // A maximally confident claim, so every test below proves the claim did not
            // decide the check (C2).
            contract_status: ContractStatus::Satisfied,
            deviations: Vec::new(),
            follow_ups: Vec::new(),
            evidence: None,
            duration_secs: 1,
            source: ReportSource::Agent,
        }
    }

    fn digests(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    fn find<'a>(result: &'a VerificationResult, name: &str) -> &'a CheckResult {
        result
            .checks
            .iter()
            .find(|c| c.name == name)
            .unwrap_or_else(|| panic!("no check named {name} in {:?}", result.checks.iter().map(|c| &c.name).collect::<Vec<_>>()))
    }

    /// The digest the freeze would have recorded for the given `api.md` contents.
    fn frozen_digest_for(contents: &str) -> String {
        format!("sha256:{:x}", Sha256::digest(contents.as_bytes()))
    }

    #[tokio::test]
    async fn missing_artifact_fails_existence() {
        let tmp = TempDir::new("missing");
        let contract = contract(vec![artifact()]);
        let report = report(Vec::new());
        let frozen = digests(&[("job-store", "sha256:whatever")]);
        let ctx = VerifyContext {
            contract: &contract,
            report: &report,
            workspace: &tmp.path,
            frozen_digests: &frozen,
        };

        let result = HiveVerifier::new().verify(&ctx).await.expect("verify");
        let existence = find(&result, "existence:job-store");
        assert!(!existence.passed, "a nonexistent artifact must fail check 1");
        assert!(
            existence.evidence.contains("src/store"),
            "evidence must name the path: {}",
            existence.evidence
        );
        assert!(!result.passed);
    }

    #[tokio::test]
    async fn digest_mismatch_fails_and_quotes_both_digests() {
        let tmp = TempDir::new("drift");
        tmp.write("src/store/api.md", "fn submit_job(spec: Spec) -> JobId;\n");

        let frozen_digest = frozen_digest_for("fn submit_job(spec: Spec) -> JobId;\n");
        // The worker then edited the frozen file without asking (§9.2).
        tmp.write("src/store/api.md", "fn submit_job(spec: Spec, prio: u8) -> JobId;\n");
        let actual_digest = frozen_digest_for("fn submit_job(spec: Spec, prio: u8) -> JobId;\n");

        let contract = contract(vec![artifact()]);
        let report = report(Vec::new());
        let frozen = digests(&[("job-store", frozen_digest.as_str())]);
        let ctx = VerifyContext {
            contract: &contract,
            report: &report,
            workspace: &tmp.path,
            frozen_digests: &frozen,
        };

        let result = HiveVerifier::new().verify(&ctx).await.expect("verify");
        let check = find(&result, "interface-frozen:job-store");
        assert!(!check.passed, "an edited frozen file must fail check 3");
        assert!(
            check.evidence.contains(&frozen_digest),
            "evidence must quote the frozen digest: {}",
            check.evidence
        );
        assert!(
            check.evidence.contains(&actual_digest),
            "evidence must quote the recomputed digest: {}",
            check.evidence
        );
        assert!(check.evidence.contains("api.md"), "evidence must name the file");
        assert!(!result.passed, "the report claimed `satisfied`; the digest says otherwise");
    }

    #[tokio::test]
    async fn matching_digest_passes_interface_freeze() {
        let tmp = TempDir::new("frozen-ok");
        let api = "fn submit_job(spec: Spec) -> JobId;\n";
        tmp.write("src/store/api.md", api);

        let contract = contract(vec![artifact()]);
        let report = report(Vec::new());
        let frozen = digests(&[("job-store", frozen_digest_for(api).as_str())]);
        let ctx = VerifyContext {
            contract: &contract,
            report: &report,
            workspace: &tmp.path,
            frozen_digests: &frozen,
        };

        let result = HiveVerifier::new().verify(&ctx).await.expect("verify");
        let check = find(&result, "interface-frozen:job-store");
        assert!(check.passed, "unchanged frozen files must pass: {}", check.evidence);
        assert!(check.evidence.contains("sha256:"));
    }

    #[tokio::test]
    async fn symbol_grep_finds_and_misses() {
        let tmp = TempDir::new("symbols");
        tmp.write("src/store/api.md", "fn submit_job(spec: Spec) -> JobId;\n");
        tmp.write("src/store/lib.rs", "pub fn submit_job() {}\n");
        // Build output must not be searched: a stale copy of a removed symbol there
        // would make check 5 pass on an artifact that no longer has it.
        tmp.write("src/store/target/old.rs", "pub fn get_status() {}\n");

        let mut spec = artifact();
        spec.symbols = vec!["submit_job".into(), "get_status".into()];
        let both = contract(vec![spec]);
        let report = report(Vec::new());
        let frozen = digests(&[(
            "job-store",
            frozen_digest_for("fn submit_job(spec: Spec) -> JobId;\n").as_str(),
        )]);
        let ctx = VerifyContext {
            contract: &both,
            report: &report,
            workspace: &tmp.path,
            frozen_digests: &frozen,
        };

        let result = HiveVerifier::new().verify(&ctx).await.expect("verify");
        let check = find(&result, "symbols:job-store");
        assert!(!check.passed, "a symbol only present in target/ must not count as found");
        assert!(check.evidence.contains("submit_job"), "found symbol must be named");
        assert!(check.evidence.contains("api.md") || check.evidence.contains("lib.rs"));
        assert!(check.evidence.contains("get_status"), "missing symbol must be named");
        assert!(
            check.evidence.contains("literal grep"),
            "check 5 must state its own shallowness: {}",
            check.evidence
        );

        // And the positive case, with the same fixture minus the missing symbol.
        let mut spec = artifact();
        spec.symbols = vec!["submit_job".into()];
        let narrowed = contract(vec![spec]);
        let ctx = VerifyContext {
            contract: &narrowed,
            report: &report,
            workspace: &tmp.path,
            frozen_digests: &frozen,
        };
        let result = HiveVerifier::new().verify(&ctx).await.expect("verify");
        let check = find(&result, "symbols:job-store");
        assert!(check.passed, "a present symbol must be found: {}", check.evidence);
        assert!(check.evidence.contains("not proof of semantics"));
    }

    #[tokio::test]
    async fn integrity_checks_the_claim_and_skips_visibly_without_one() {
        let tmp = TempDir::new("integrity");
        let body = "{\"jobs\": []}\n";
        tmp.write("state.json", body);

        let mut spec = artifact();
        spec.path = "state.json".into();
        spec.interface_files = Vec::new();
        spec.symbols = Vec::new();
        let contract = contract(vec![spec]);
        let frozen = BTreeMap::new();

        // A wrong claim fails, and both digests appear.
        let lying = report(vec![ReportArtifact {
            artifact_id: "job-store".into(),
            path: "state.json".into(),
            sha256: Some("sha256:0000000000000000000000000000000000000000000000000000000000000000".into()),
            exists: true,
        }]);
        let ctx = VerifyContext {
            contract: &contract,
            report: &lying,
            workspace: &tmp.path,
            frozen_digests: &frozen,
        };
        let result = HiveVerifier::new().verify(&ctx).await.expect("verify");
        let check = find(&result, "integrity:job-store");
        assert!(!check.passed);
        assert!(check.evidence.contains("sha256:0000"));
        assert!(check.evidence.contains(&format!("sha256:{:x}", Sha256::digest(body.as_bytes()))));

        // No claim: a clean skip that still says what it did not do.
        let silent = report(Vec::new());
        let ctx = VerifyContext {
            contract: &contract,
            report: &silent,
            workspace: &tmp.path,
            frozen_digests: &frozen,
        };
        let result = HiveVerifier::new().verify(&ctx).await.expect("verify");
        let check = find(&result, "integrity:job-store");
        assert!(check.passed);
        assert!(
            check.evidence.contains("claims no sha256"),
            "a skip must be visible in the verdict: {}",
            check.evidence
        );
    }

    #[tokio::test]
    async fn build_probe_runs_the_contract_command() {
        let tmp = TempDir::new("probe");
        tmp.write("src/store/api.md", "submit_job\n");
        let digest = frozen_digest_for("submit_job\n");

        let mut spec = artifact();
        spec.check = Some(ContractCheck {
            kind: "command".into(),
            command: "echo building; exit 3".into(),
        });
        let contract = contract(vec![spec]);
        let report = report(Vec::new());
        let frozen = digests(&[("job-store", digest.as_str())]);
        let ctx = VerifyContext {
            contract: &contract,
            report: &report,
            workspace: &tmp.path,
            frozen_digests: &frozen,
        };

        let result = HiveVerifier::new().verify(&ctx).await.expect("verify");
        let check = find(&result, "build-probe:job-store");
        assert!(!check.passed, "a nonzero exit must fail check 4");
        assert!(check.evidence.contains("exit 3"), "evidence must quote the status");
        assert!(check.evidence.contains("building"), "evidence must quote the output tail");
    }

    #[tokio::test]
    async fn schema_check_reports_violations_and_its_own_limits() {
        let tmp = TempDir::new("schema");
        tmp.write("state.json", "{\"id\": 7}");

        let mut spec = artifact();
        spec.path = "state.json".into();
        spec.format = ArtifactFormat::Json;
        spec.interface_files = Vec::new();
        spec.symbols = Vec::new();
        spec.schema = Some(serde_json::json!({
            "type": "object",
            "required": ["id", "name"],
            "properties": {"id": {"type": "string"}},
            "$ref": "#/definitions/other"
        }));
        let contract = contract(vec![spec]);
        let report = report(Vec::new());
        let frozen = BTreeMap::new();
        let ctx = VerifyContext {
            contract: &contract,
            report: &report,
            workspace: &tmp.path,
            frozen_digests: &frozen,
        };

        let result = HiveVerifier::new().verify(&ctx).await.expect("verify");
        let check = find(&result, "schema:job-store");
        assert!(!check.passed);
        assert!(check.evidence.contains("name"), "the missing property must be named");
        assert!(check.evidence.contains("expected type"), "the type error must be named");
        assert!(
            check.evidence.contains("NOT ENFORCED") && check.evidence.contains("$ref"),
            "the validator must declare the keywords it ignored: {}",
            check.evidence
        );
    }

    #[test]
    fn acceptance_test_contains_the_example_values() {
        let mut spec = artifact();
        spec.check = Some(ContractCheck {
            kind: "command".into(),
            command: "./store submit".into(),
        });
        spec.examples = vec![
            ExamplePair {
                input: Value::String("submit echo-hi".into()),
                output: Value::String("job-1".into()),
            },
            ExamplePair {
                input: Value::String("submit 'quoted'".into()),
                output: Value::String("job-2".into()),
            },
        ];
        let mut contract = contract(vec![spec]);
        contract.integration = Some(IntegrationSpec { command: "make test".into() });

        let script = HiveVerifier::new().acceptance_test(&contract).expect("generate");

        assert!(script.starts_with("#!/bin/sh"));
        assert!(script.contains("'submit echo-hi'"), "input must appear verbatim:\n{script}");
        assert!(script.contains("'job-1'"), "output must appear verbatim:\n{script}");
        assert!(script.contains("'job-2'"));
        assert!(script.contains("'./store submit'"), "the check command must be what runs");
        assert!(
            script.contains("'submit '\\''quoted'\\'''"),
            "a quote in an example must survive shell quoting:\n{script}"
        );
        assert!(script.contains("c-test"), "the script must name the contract it came from");
        assert_eq!(script.matches("assert_example '").count(), 2);
    }

    #[test]
    fn acceptance_test_says_so_when_examples_cannot_run() {
        let mut spec = artifact();
        spec.check = None;
        spec.examples = vec![ExamplePair {
            input: Value::String("in".into()),
            output: Value::String("out".into()),
        }];
        let script = HiveVerifier::new()
            .acceptance_test(&contract(vec![spec]))
            .expect("generate");
        assert!(script.contains("SKIPPED"), "an unrunnable example must be visible:\n{script}");
        assert!(script.contains("no check.command"));
    }

    #[tokio::test]
    async fn integration_runs_the_command_and_the_acceptance_test() {
        let tmp = TempDir::new("integrate");

        let mut spec = artifact();
        spec.check = Some(ContractCheck {
            kind: "command".into(),
            command: "cat".into(),
        });
        spec.examples = vec![ExamplePair {
            input: Value::String("job-1".into()),
            output: Value::String("job-1".into()),
        }];
        let mut c = contract(vec![spec]);
        c.integration = Some(IntegrationSpec { command: "true".into() });

        let check = HiveVerifier::new().integrate(&c, &tmp.path).await.expect("integrate");
        assert!(check.passed, "cat echoes its input, so the example holds: {}", check.evidence);
        assert!(check.evidence.contains("1 of 1 example(s) passed"));
        assert!(tmp.path.join(ACCEPTANCE_TEST_PATH).exists(), "the script must be retained");

        // Now an example the artifact does not satisfy.
        let mut spec = artifact();
        spec.check = Some(ContractCheck { kind: "command".into(), command: "cat".into() });
        spec.examples = vec![ExamplePair {
            input: Value::String("job-1".into()),
            output: Value::String("job-2".into()),
        }];
        let mut c = contract(vec![spec]);
        c.integration = Some(IntegrationSpec { command: "true".into() });

        let check = HiveVerifier::new().integrate(&c, &tmp.path).await.expect("integrate");
        assert!(!check.passed, "a broken example must fail integration");
        assert!(check.evidence.contains("FAIL"));
        assert!(check.evidence.contains("job-2"), "evidence must quote what was expected");
    }

    #[tokio::test]
    async fn integration_failure_short_circuits_before_the_acceptance_test() {
        let tmp = TempDir::new("integrate-fail");
        let mut c = contract(Vec::new());
        c.integration = Some(IntegrationSpec {
            command: "echo linker exploded >&2; exit 1".into(),
        });

        let check = HiveVerifier::new().integrate(&c, &tmp.path).await.expect("integrate");
        assert!(!check.passed);
        assert!(check.evidence.contains("linker exploded"), "output tail must be quoted");
        assert!(check.evidence.contains("not run"));
    }

    #[tokio::test]
    async fn a_report_for_a_role_that_produces_nothing_is_not_a_silent_pass() {
        let tmp = TempDir::new("vacuous");
        let contract = contract(Vec::new());
        let report = report(Vec::new());
        let frozen = BTreeMap::new();
        let ctx = VerifyContext {
            contract: &contract,
            report: &report,
            workspace: &tmp.path,
            frozen_digests: &frozen,
        };

        let result = HiveVerifier::new().verify(&ctx).await.expect("verify");
        assert_eq!(result.checks.len(), 1);
        assert!(result.checks[0].evidence.contains("asserts nothing about the work"));
    }

    #[tokio::test]
    async fn an_undeclared_artifact_claim_fails() {
        let tmp = TempDir::new("undeclared");
        let contract = contract(Vec::new());
        let report = report(vec![ReportArtifact {
            artifact_id: "surprise".into(),
            path: "src/surprise".into(),
            sha256: None,
            exists: true,
        }]);
        let frozen = BTreeMap::new();
        let ctx = VerifyContext {
            contract: &contract,
            report: &report,
            workspace: &tmp.path,
            frozen_digests: &frozen,
        };

        let result = HiveVerifier::new().verify(&ctx).await.expect("verify");
        let check = find(&result, "existence:surprise");
        assert!(!check.passed);
        assert!(check.evidence.contains("contract.change.requested"));
    }

    #[test]
    fn command_output_tail_is_capped_on_a_char_boundary() {
        let long = "é".repeat(OUTPUT_TAIL_BYTES);
        let tail = tail_of(&long, 64);
        assert!(tail.contains("earlier bytes omitted"));
        assert!(tail.len() < OUTPUT_TAIL_BYTES);
    }
}
