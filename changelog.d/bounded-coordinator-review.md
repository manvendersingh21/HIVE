### Fixed

- Coordinator review is bounded per task. After three consecutive `continue`
  reviews with no new acceptance evidence the task is set to `blocked` with a
  summary and no further messages are sent, instead of asking the same workers
  for the same missing evidence forever. The count lives in the review claim
  table, so it survives a restart and resets once a task is settled.
- A follow-up message that cannot be delivered is dropped on its own, logged
  with its `task_id`, `run_id` and reason, instead of failing the whole review.
  Only a verdict whose messages are *all* unusable still fails. The coordinator
  review warning now names the `task_id` it was reviewing.
- Review sees what OpenCode and AGY runs actually did. Native evidence is
  extracted from OpenCode's terminal assistant message and bash tool output and
  from an AGY stream-json event whose `event` is `"result"`, reading its
  `result.response`, alongside the Claude and Codex shapes, so a finished run no
  longer looks like it produced nothing.
- Review evidence is capped to the newest output. A long run keeps its final
  report, test results and branch reference and loses its beginning, behind an
  `[earlier output truncated]` marker.
- A review answer is retried once whether it was not valid JSON or not a valid
  review object, with the exact failure appended to the prompt.
