### Fixed

- Structured (schema) calls to Z.AI now send `response_format: {"type": "json_object"}`,
  so GLM returns syntactically valid JSON for plans. Structured calls to NVIDIA Nemotron 3
  models (`nvidia/nemotron-*`), for which NVIDIA documents JSON mode, send it too,
  streaming or not. Plain chat calls, and other configured NVIDIA models, are unchanged.
- A plan that is not valid JSON, or does not match the plan schema, is retried once
  with the parser error and the offending excerpt appended to the prompt ("Your previous
  answer was invalid JSON at line 1 column N: …"). The retry shares the existing planning
  deadline. Two malformed answers fail with a clear message saying no commands were
  executed. This applies to both the chat planner and the delegation planner.
- A malformed plan is logged at warn level with its conversation ID, line, column and a
  bounded (about 200 characters around the failure) excerpt with credential-shaped text
  redacted. The full model answer is never logged.
- A restart during planning no longer claims "Some commands may have run". Planning has
  no side effects, so at startup a turn that was still planning is planned again, at most
  once, with the message "Planning was interrupted by a restart and has been restarted."
  A turn interrupted again while re-planning is marked interrupted instead. Executing
  turns keep the existing interrupted behaviour, and a delegation turn is marked
  executing before it creates containers or runs.
