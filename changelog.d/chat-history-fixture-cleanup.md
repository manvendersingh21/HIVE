### Fixed
- The disposable saved-chat integration fixture shadows native agent executables in its temporary home and stops its own process group before cleanup, so background inventory probes cannot start installed agents or race removal of the fixture directory.
