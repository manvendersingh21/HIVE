### Fixed

- OpenCode turns now end when OpenCode compacts the context or auto-continues mid-turn: a completed reply parented to any user message OpenCode created after the runner prompt ends the turn once the session is idle, and actions are counted across that whole chain. Previously such follow-up turns stayed `working` forever and new inbox messages were never processed.
