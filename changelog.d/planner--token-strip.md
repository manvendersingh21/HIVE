### Fixed

- Plans made with Z.AI no longer lose every lowercase `json`: `corpus.jsonl` no longer
  becomes `corpus.l`, and `import json;json.loads` no longer becomes `import ;.loads`. GLM's JSON mode (`response_format: json_object`)
  deletes that token from its answers (zai-org/GLM-5#133), so structured calls to Z.AI no
  longer request it. The schema instructions in the prompt, JSON extraction and the one
  corrective retry for a malformed plan still apply.
