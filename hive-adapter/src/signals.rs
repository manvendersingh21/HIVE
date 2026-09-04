//! Deadline enforcement: suspend, never kill (`spec/HACP.md` §13.3, §15).
//!
//! An adapter that reaches its deadline stops the worker's process group with SIGSTOP.
//! It does not terminate it. The reason is not politeness: a run that hits a deadline is
//! a run a human is about to be asked to judge, and killing the worker destroys the
//! process state, open files, and scrollback that the judgement depends on. A stopped
//! process can be inspected and then continued with SIGCONT; a killed one cannot be
//! anything.

/// A validated process-group id.
///
/// The newtype exists so that an unchecked integer cannot reach [`suspend`]. `killpg`
/// interprets small and negative values specially — `0` means *the caller's own process
/// group*, so the adapter would stop itself, and `-1` means *every process the user has
/// permission to signal*, which on a developer's machine is their entire session. Both
/// of those are one typo away from a plain integer argument, so the type makes them
/// unrepresentable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pgid(i32);

impl Pgid {
    pub fn get(&self) -> i32 {
        self.0
    }
}

impl std::fmt::Display for Pgid {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Parse and guard a process-group id given as text.
///
/// Requires a non-empty, all-ASCII-digit string greater than 1. Digits-only is what
/// excludes `-1` before any arithmetic happens, `> 1` excludes both `0` (the adapter's
/// own group) and `1` (init, or on macOS launchd), and the emptiness check exists
/// because an unset shell variable expands to an empty argument rather than to no
/// argument at all — the single most likely way a bad value gets here in practice.
pub fn parse_pgid(raw: &str) -> Result<Pgid, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err("process group id is empty".to_string());
    }
    if !trimmed.bytes().all(|b| b.is_ascii_digit()) {
        return Err(format!(
            "process group id {trimmed:?} must be digits only; a negative value would signal \
             more processes than the worker"
        ));
    }
    let value: i32 = trimmed
        .parse()
        .map_err(|_| format!("process group id {trimmed:?} does not fit in a pid"))?;
    if value <= 1 {
        return Err(format!(
            "process group id {value} is refused: 0 is the adapter's own group and 1 is init"
        ));
    }
    Ok(Pgid(value))
}

/// Stop every process in the group. Returns the OS error on failure.
///
/// Resuming is deliberately not implemented here: a stopped run is stopped so a human
/// can look at it, and that human continues it with `kill -CONT -<pgid>` once they have.
/// An adapter that could un-stop the worker would eventually be made to do so on a
/// timer, which is the same as not having stopped it.
///
/// Limitation worth stating plainly: this suspends the group the adapter was *told*
/// about. If the worker has since moved a child into a different process group — a
/// shell running a pipeline in job-control mode, say — that child keeps running. The
/// adapter cannot discover this without walking the process tree, which it deliberately
/// does not do.
pub fn suspend(pgid: Pgid) -> Result<(), std::io::Error> {
    signal_group(pgid, libc::SIGSTOP)
}

/// Whether any process in the group still exists.
///
/// Signal 0 performs the permission and existence checks without delivering anything.
/// `EPERM` means the group exists but is not ours to signal, which still answers the
/// question being asked here.
pub fn group_alive(pgid: Pgid) -> bool {
    match signal_group(pgid, 0) {
        Ok(()) => true,
        Err(e) => e.raw_os_error() == Some(libc::EPERM),
    }
}

fn signal_group(pgid: Pgid, signal: libc::c_int) -> Result<(), std::io::Error> {
    // SAFETY: `pgid` is a validated group id greater than 1, so this cannot resolve to
    // the caller's own group (0) or to the broadcast group (-1). `killpg` has no memory
    // effects; the only unsafety is the FFI call itself.
    let rc = unsafe { libc::killpg(pgid.get(), signal) };
    if rc == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_the_dangerous_values() {
        // 0 is "my own process group": the adapter would stop itself and stop relaying.
        assert!(parse_pgid("0").is_err());
        // -1 is "every process I may signal": the user's whole session.
        assert!(parse_pgid("-1").is_err());
        // An unset shell variable expands to this.
        assert!(parse_pgid("").is_err());
        assert!(parse_pgid("   ").is_err());
        // 1 is init/launchd.
        assert!(parse_pgid("1").is_err());
    }

    #[test]
    fn rejects_values_that_are_not_plain_digits() {
        assert!(parse_pgid("12a").is_err());
        assert!(parse_pgid("+42").is_err());
        assert!(parse_pgid("4 2").is_err());
        assert!(parse_pgid("99999999999999999999").is_err());
    }

    #[test]
    fn sees_a_live_process_group() {
        // The test process's own group is the one group guaranteed to exist. This also
        // demonstrates why the guard matters: `getpgrp()` is a plain integer, and
        // nothing but the guard stands between a stray 0 here and SIGSTOP to self.
        let own = unsafe { libc::getpgrp() };
        let pgid = parse_pgid(&own.to_string()).expect("the test's own group id is valid");
        assert!(group_alive(pgid));
    }

    #[test]
    fn accepts_a_real_group_id() {
        assert_eq!(parse_pgid("4321").map(|p| p.get()), Ok(4321));
        // Surrounding whitespace is a file/argument artifact, not a different value.
        assert_eq!(parse_pgid(" 4321\n").map(|p| p.get()), Ok(4321));
    }
}
