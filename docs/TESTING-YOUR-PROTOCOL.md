# Protocol testing has moved

The HACP testing playbook is maintained with the independent library:

[HACP protocol testing guide](https://github.com/manvendersingh21/hacp/blob/main/docs/TESTING-YOUR-PROTOCOL.md).

Run protocol tests from a checkout of that repository. HIVE's own tests exercise
the runtime against a commit-pinned HACP dependency; they do not run dependency
unit tests. See [HACP integration](HACP-HIVE.md#tests-after-separation).

Runtime-specific live tests and their limitations remain in
[Release 1](RELEASE-1.md#final-acceptance-audit--complete-2026-09-07).
