//! High-Risk Command Interceptor — extends the Tier-1 watchdog rules with
//! diff-threshold analysis and bulk-deletion pattern detection.
//!
//! This module is the "before-execution" gate for the agent: it takes a
//! command string (or a [`DiffImpact`] representing a batch of file writes)
//! and decides whether execution should be paused for human approval.
//!
//! It does **not** prompt the user or send notifications itself — that is the
//! caller's job (the CLI does it in `approval.rs`, the web UI does it via
//! the `/api/chat/:run_id/approve` flow). This module only answers the
//! question "should this be gated?"

use hive_common::{SafetyAnalysis, SafetyCategory, Severity};
use regex::Regex;
use serde::{Deserialize, Serialize};
use tracing::debug;

use super::Watchdog;

// ── Thresholds ──────────────────────────────────────────────────────────

/// Default maximum number of files a single operation may affect before
/// the interceptor triggers.
pub const DEFAULT_MAX_FILES: usize = 5;

/// Default maximum number of lines that may be deleted before the
/// interceptor triggers.
pub const DEFAULT_MAX_LINES_DELETED: usize = 100;

// ── Types ───────────────────────────────────────────────────────────────

/// Why the interceptor flagged an action.
#[derive(Debug, Clone)]
pub enum InterceptReason {
    /// Matched a Tier-1 watchdog regex rule.
    WatchdogRule(SafetyAnalysis),
    /// Exceeds the diff threshold for files affected or lines deleted.
    DiffThreshold {
        files_affected: usize,
        lines_deleted: usize,
        max_files: usize,
        max_lines_deleted: usize,
    },
    /// Matches a bulk-deletion pattern not covered by existing Tier-1 rules.
    BulkDeletion(String),
}

impl InterceptReason {
    /// One-line summary suitable for console display and iMessage.
    pub fn summary(&self) -> String {
        match self {
            Self::WatchdogRule(a) => a.reason.clone(),
            Self::DiffThreshold {
                files_affected,
                lines_deleted,
                ..
            } => format!(
                "Operation affects {files_affected} file(s) with {lines_deleted} line(s) \
                 deleted — exceeds safety thresholds"
            ),
            Self::BulkDeletion(detail) => {
                format!("Bulk deletion pattern detected: {detail}")
            }
        }
    }

    pub fn severity(&self) -> Severity {
        match self {
            Self::WatchdogRule(a) => a.severity,
            Self::DiffThreshold { .. } => Severity::High,
            Self::BulkDeletion(_) => Severity::High,
        }
    }

    /// Convert into a [`SafetyAnalysis`] for integration with the existing
    /// plan/approval pipeline (which gates on `PlannedStep::risk`).
    pub fn to_safety_analysis(&self) -> SafetyAnalysis {
        SafetyAnalysis {
            is_safe: false,
            severity: self.severity(),
            category: Some(match self {
                Self::WatchdogRule(a) => a
                    .category
                    .clone()
                    .unwrap_or(SafetyCategory::DestructiveCommand),
                Self::DiffThreshold { .. } => SafetyCategory::DestructiveCommand,
                Self::BulkDeletion(_) => SafetyCategory::DestructiveCommand,
            }),
            reason: self.summary(),
            suggested_action:
                "Pause execution and have a human review before proceeding.".to_string(),
        }
    }
}

/// Measures the impact of a file operation or refactor. Built by callers
/// before invoking the interceptor (e.g. by running `git diff --numstat`
/// or by counting the files a plan touches).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DiffImpact {
    pub files_affected: usize,
    pub lines_deleted: usize,
    pub lines_added: usize,
    pub file_list: Vec<String>,
}

// ── Interceptor ─────────────────────────────────────────────────────────

/// High-risk command interceptor. Combines the existing `Watchdog` Tier-1
/// regex rules with additional bulk-deletion patterns and diff-threshold
/// analysis.
///
/// Stateless beyond its compiled regexes and threshold configuration.
pub struct Interceptor {
    pub max_files: usize,
    pub max_lines_deleted: usize,
    bulk_patterns: Vec<(Regex, &'static str)>,
}

impl Default for Interceptor {
    fn default() -> Self {
        Self::new(DEFAULT_MAX_FILES, DEFAULT_MAX_LINES_DELETED)
    }
}

impl Interceptor {
    /// Create an interceptor with explicit thresholds.
    pub fn new(max_files: usize, max_lines_deleted: usize) -> Self {
        // Compile all bulk-deletion patterns once at construction time.
        let bulk_patterns = vec![
            (
                Regex::new(r"find\s+.*-delete").expect("regex"),
                "find with -delete flag",
            ),
            (
                Regex::new(r"find\s+.*-exec\s+rm").expect("regex"),
                "find with -exec rm",
            ),
            (
                Regex::new(r"xargs\s+.*rm\b").expect("regex"),
                "xargs piped to rm",
            ),
            (
                // git clean with destructive flags (-f, -d, -x).
                // We match the shape and exclude --dry-run / -n in code
                // below, because the regex crate has no negative lookahead.
                Regex::new(r"git\s+clean\s+.*-\w*[fdx]").expect("regex"),
                "git clean with destructive flags",
            ),
            (
                // rm with wildcard glob — e.g. `rm *.py`, `rm src/*.rs`
                Regex::new(r"rm\s+(-\w+\s+)*\S*\*").expect("regex"),
                "rm with wildcard pattern",
            ),
        ];
        Self {
            max_files,
            max_lines_deleted,
            bulk_patterns,
        }
    }

    // ── Command checking ────────────────────────────────────────────

    /// Check a shell command against Tier-1 watchdog rules AND the
    /// interceptor's additional bulk-deletion patterns.
    ///
    /// Returns `None` if the command looks safe. Returns the first
    /// matching reason otherwise (highest-severity watchdog match wins
    /// over a bulk-deletion match).
    pub fn check_command(
        &self,
        watchdog: &Watchdog,
        command: &str,
    ) -> Option<InterceptReason> {
        // 1. Existing Tier-1 watchdog rules (severity >= Medium)
        if let Some(analysis) = watchdog.scan_line(command) {
            if analysis.severity >= Severity::Medium {
                debug!(command, rule = %analysis.reason, "Interceptor: Tier-1 rule hit");
                return Some(InterceptReason::WatchdogRule(analysis));
            }
        }

        // 2. Additional bulk-deletion patterns
        for (pattern, description) in &self.bulk_patterns {
            if pattern.is_match(command) {
                // git clean with --dry-run / -n is safe; skip it.
                if *description == "git clean with destructive flags"
                    && (command.contains("--dry-run") || command.contains(" -n"))
                {
                    continue;
                }
                debug!(command, pattern = description, "Interceptor: bulk deletion hit");
                return Some(InterceptReason::BulkDeletion(description.to_string()));
            }
        }

        None
    }

    // ── Diff threshold checking ─────────────────────────────────────

    /// Check whether a [`DiffImpact`] exceeds the configured thresholds.
    ///
    /// Returns `None` if within limits.
    pub fn check_diff_impact(&self, impact: &DiffImpact) -> Option<InterceptReason> {
        let files_exceeded = impact.files_affected > self.max_files;
        let lines_exceeded = impact.lines_deleted > self.max_lines_deleted;

        if files_exceeded || lines_exceeded {
            debug!(
                files = impact.files_affected,
                deleted = impact.lines_deleted,
                max_files = self.max_files,
                max_deleted = self.max_lines_deleted,
                "Interceptor: diff threshold exceeded"
            );
            Some(InterceptReason::DiffThreshold {
                files_affected: impact.files_affected,
                lines_deleted: impact.lines_deleted,
                max_files: self.max_files,
                max_lines_deleted: self.max_lines_deleted,
            })
        } else {
            None
        }
    }

    // ── Combined assessment ─────────────────────────────────────────

    /// Full assessment: check the command against Tier-1 rules AND bulk
    /// deletion patterns, then return a [`SafetyAnalysis`] suitable for
    /// the plan/approval pipeline.
    ///
    /// This is the drop-in replacement for `assess_command` in
    /// `agent/run.rs` that adds interceptor coverage.
    pub fn assess(
        &self,
        watchdog: &Watchdog,
        command: &str,
    ) -> Option<SafetyAnalysis> {
        self.check_command(watchdog, command)
            .map(|reason| reason.to_safety_analysis())
    }
}

// ── Diff impact helpers ─────────────────────────────────────────────────

/// Compute the diff impact of a git working tree by running
/// `git diff --numstat HEAD`. Returns an empty impact on failure.
pub async fn compute_git_diff_impact(
    repo_dir: Option<&str>,
) -> DiffImpact {
    let mut cmd = tokio::process::Command::new("git");
    cmd.args(["diff", "--numstat", "HEAD"]);
    if let Some(dir) = repo_dir {
        cmd.current_dir(dir);
    }

    let output = match cmd.output().await {
        Ok(o) if o.status.success() => o,
        _ => return DiffImpact::default(),
    };

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut impact = DiffImpact::default();

    for line in stdout.lines() {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() >= 3 {
            // binary files report `-` instead of a number
            let added: usize = parts[0].parse().unwrap_or(0);
            let deleted: usize = parts[1].parse().unwrap_or(0);
            let file = parts[2].to_string();

            impact.lines_added += added;
            impact.lines_deleted += deleted;
            impact.files_affected += 1;
            impact.file_list.push(file);
        }
    }

    impact
}

/// Count how many distinct files a list of shell commands might affect.
/// This is a heuristic — it looks for file paths in write-style commands.
pub fn estimate_files_affected(commands: &[String]) -> usize {
    let write_patterns = Regex::new(
        r"(?:>\s*|tee\s+|cp\s+.*\s+|mv\s+.*\s+|rm\s+)(\S+)"
    ).expect("regex");

    let mut files = std::collections::HashSet::new();
    for cmd in commands {
        for cap in write_patterns.captures_iter(cmd) {
            if let Some(path) = cap.get(1) {
                files.insert(path.as_str().to_string());
            }
        }
    }
    files.len()
}

// ── Console display helpers ─────────────────────────────────────────────

/// Print a rich console summary of an intercepted action.
///
/// Called by the CLI approval gate and the full `intercept()` flow.
pub fn print_intercept_summary(command: &str, reason: &InterceptReason) {
    eprintln!();
    eprintln!("╔══════════════════════════════════════════════════════════════════╗");
    eprintln!("║             ⚠️  HIGH-RISK ACTION INTERCEPTED  ⚠️                 ║");
    eprintln!("╠══════════════════════════════════════════════════════════════════╣");
    eprintln!("║  Status: AwaitingHumanApproval                                 ║");
    eprintln!("╚══════════════════════════════════════════════════════════════════╝");
    eprintln!();
    eprintln!("  Command:   {command}");
    eprintln!("  Severity:  {}", reason.severity());
    eprintln!("  Reason:    {}", reason.summary());

    match reason {
        InterceptReason::DiffThreshold {
            files_affected,
            lines_deleted,
            max_files,
            max_lines_deleted,
        } => {
            eprintln!();
            eprintln!("  Diff Impact:");
            eprintln!(
                "    Files affected:  {files_affected} (threshold: {max_files}){}",
                if *files_affected > *max_files { " ← EXCEEDED" } else { "" }
            );
            eprintln!(
                "    Lines deleted:   {lines_deleted} (threshold: {max_lines_deleted}){}",
                if *lines_deleted > *max_lines_deleted { " ← EXCEEDED" } else { "" }
            );
        }
        InterceptReason::WatchdogRule(analysis) => {
            if let Some(cat) = &analysis.category {
                eprintln!("  Category:  {cat}");
            }
        }
        InterceptReason::BulkDeletion(_) => {}
    }

    eprintln!();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn interceptor() -> Interceptor {
        Interceptor::default()
    }

    fn watchdog() -> Watchdog {
        Watchdog::new()
    }

    // ── Command checks ──────────────────────────────────────────────

    #[test]
    fn catches_rm_rf_via_watchdog() {
        let i = interceptor();
        let w = watchdog();
        let r = i.check_command(&w, "rm -rf /tmp/important");
        assert!(r.is_some());
        assert!(matches!(r.unwrap(), InterceptReason::WatchdogRule(_)));
    }

    #[test]
    fn catches_git_reset_hard() {
        let i = interceptor();
        let w = watchdog();
        assert!(i.check_command(&w, "git reset --hard HEAD~3").is_some());
    }

    #[test]
    fn catches_git_clean_fd() {
        let i = interceptor();
        let w = watchdog();
        assert!(i.check_command(&w, "git clean -fd").is_some());
    }

    #[test]
    fn catches_find_delete() {
        let i = interceptor();
        let w = watchdog();
        let r = i.check_command(&w, "find /tmp -name '*.log' -delete");
        assert!(r.is_some());
        assert!(matches!(r.unwrap(), InterceptReason::BulkDeletion(_)));
    }

    #[test]
    fn catches_find_exec_rm() {
        let i = interceptor();
        let w = watchdog();
        let r = i.check_command(&w, "find . -name '*.bak' -exec rm {} +");
        assert!(r.is_some());
    }

    #[test]
    fn catches_xargs_rm() {
        let i = interceptor();
        let w = watchdog();
        let r = i.check_command(&w, "find . -name '*.tmp' | xargs rm");
        assert!(r.is_some());
    }

    #[test]
    fn catches_rm_with_wildcard() {
        let i = interceptor();
        let w = watchdog();
        let r = i.check_command(&w, "rm *.py");
        assert!(r.is_some());
    }

    #[test]
    fn passes_benign_commands() {
        let i = interceptor();
        let w = watchdog();
        assert!(i.check_command(&w, "ls -la").is_none());
        assert!(i.check_command(&w, "git status").is_none());
        assert!(i.check_command(&w, "cargo build").is_none());
        assert!(i.check_command(&w, "cat README.md").is_none());
    }

    // ── Diff threshold checks ───────────────────────────────────────

    #[test]
    fn flags_when_files_exceed_threshold() {
        let i = interceptor();
        let impact = DiffImpact {
            files_affected: 10,
            lines_deleted: 50,
            lines_added: 20,
            file_list: (0..10).map(|n| format!("file{n}.rs")).collect(),
        };
        assert!(i.check_diff_impact(&impact).is_some());
    }

    #[test]
    fn flags_when_lines_exceed_threshold() {
        let i = interceptor();
        let impact = DiffImpact {
            files_affected: 2,
            lines_deleted: 200,
            lines_added: 10,
            file_list: vec!["big.rs".into()],
        };
        assert!(i.check_diff_impact(&impact).is_some());
    }

    #[test]
    fn passes_small_diffs() {
        let i = interceptor();
        let impact = DiffImpact {
            files_affected: 3,
            lines_deleted: 20,
            lines_added: 40,
            file_list: vec!["a.rs".into(), "b.rs".into(), "c.rs".into()],
        };
        assert!(i.check_diff_impact(&impact).is_none());
    }

    #[test]
    fn custom_thresholds_work() {
        let i = Interceptor::new(2, 50);
        let impact = DiffImpact {
            files_affected: 3,
            lines_deleted: 30,
            ..Default::default()
        };
        // 3 files > 2 max_files → should flag
        assert!(i.check_diff_impact(&impact).is_some());
    }

    // ── Safety analysis conversion ──────────────────────────────────

    #[test]
    fn intercept_reason_converts_to_safety_analysis() {
        let reason = InterceptReason::BulkDeletion("find with -delete".into());
        let analysis = reason.to_safety_analysis();
        assert!(!analysis.is_safe);
        assert_eq!(analysis.severity, Severity::High);
        assert_eq!(
            analysis.category,
            Some(SafetyCategory::DestructiveCommand)
        );
    }

    // ── File estimation ─────────────────────────────────────────────

    #[test]
    fn estimate_files_counts_write_targets() {
        let commands = vec![
            "echo hello > out.txt".into(),
            "cp src/a.rs src/b.rs".into(),
            "rm old.log".into(),
        ];
        let count = estimate_files_affected(&commands);
        assert!(count >= 2, "should detect at least 2 files, got {count}");
    }
}
