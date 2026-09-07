# Security

HIVE launches agent CLIs and shell commands under the operating-system account
running it. Its watchdog, authentication checks, and protocol validation are not
a complete process sandbox or a guarantee that model-generated commands are safe.
Use disposable workspaces, least-privilege accounts, trusted SSH peers, and a
private network for terminal endpoints. Do not expose the web terminal directly
to the public Internet without an independent deployment security review.

Keep database files, WAL sidecars, logs, run journals, and provider session records
private. They may contain task data or credentials captured by diagnostics. Never
attach raw production artifacts to a public issue.

For suspected vulnerabilities, use GitHub's private vulnerability reporting option
on the repository's **Security** tab if available. If unavailable, open an issue
requesting a private reporting channel without exploit details or sensitive data.
Include the affected revision, a minimal synthetic reproduction, and the trust
boundary in the private report. No response-time guarantee is offered.

Protocol-library issues belong in
[HACP's security process](https://github.com/manvendersingh21/hcap/blob/main/SECURITY.md).
The current development branch is the maintenance target; older revisions have
no promised security support window.
