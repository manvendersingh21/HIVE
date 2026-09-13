# Summary

<!-- The problem this solves, and the approach taken. -->

## Limitations

<!--
What this does not do, and what remains unverified. Say so plainly here rather
than leaving a reviewer to discover it. "None" is a valid answer if it is true.
-->

## Validation

<!--
The exact commands you ran and what they reported. Compilation is not
validation: do not describe a roadmap item as complete on the strength of a
successful build.

If a check was skipped, say which and why.
-->

```
cargo build --workspace --locked
cargo test --workspace --locked
git diff --check
```

## Checklist

- [ ] Behavior changes come with regression tests
- [ ] The relevant guide under `docs/` is updated, or no user-facing behavior changed
- [ ] `CHANGELOG.md` updated under `## [Unreleased]`, or the change is not user-visible
- [ ] No credentials, private transcripts, real hostnames, SSH account names, or personal home paths
- [ ] Live or destructive tests, if any, ran against disposable workspaces and are identified above
- [ ] Unrelated worktree changes are not included

<!--
Protocol semantics, wire schemas, and conformance fixtures belong in the HACP
repository: https://github.com/manvendersingh21/hacp
-->
