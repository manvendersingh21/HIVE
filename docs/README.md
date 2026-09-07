# Documentation

## Current guides

- [Project overview and quick start](../README.md)
- [Distributed collaboration](DISTRIBUTED-COLLABORATION.md): roles, SSH, verification, recovery
- [HACP integration](HACP-HIVE.md): the external protocol library and dependency workflow
- [Contributing](../CONTRIBUTING.md): development and validation requirements
- [Security](../SECURITY.md): deployment boundaries and reporting
- [Release 1 acceptance](RELEASE-1.md#final-acceptance-audit--complete-2026-09-07): measured two-device results
- [Repository separation](REPOSITORY-SPLIT.md): extraction and validation evidence

## Protocol documentation

HACP's [README](https://github.com/manvendersingh21/hacp),
[specifications](https://github.com/manvendersingh21/hacp/tree/main/spec),
[schemas](https://github.com/manvendersingh21/hacp/tree/main/spec/schemas), and
[testing guide](https://github.com/manvendersingh21/hacp/blob/main/docs/TESTING-YOUR-PROTOCOL.md)
are maintained in their own repository. There is no in-tree `hacp/` crate.

## Historical records

[STATUS](STATUS.md), [PROJECT-RECORD](PROJECT-RECORD.md),
[ROADMAP](ROADMAP.md), [implementation plan](implementation-plan.md),
[placement notes](PLACEMENT.md), [deployment notes](DEPLOY-WEB.md), and
[findings](findings/) record earlier phases and maintainer-specific deployments.
Read their dates and scope: old counts, commands, machine inventories, and
uncompleted design proposals are not current installation instructions.

References to the former `hacp/` directory in historical records can be inspected
in [the pre-extraction HIVE tree](https://github.com/manvendersingh21/HIVE/tree/87dd12251a6e2375d48d77f85b64aba58b4c3d19).
Keep historical evidence intact; use the current guides for new deployments.
