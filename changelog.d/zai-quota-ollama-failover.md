### Added

- When Z.ai reports a quota error (HTTP 429, or a balance/rate-limit code such
  as 1308 "Usage limit reached for 5 hour"), the master agent answers from the
  local Ollama model (`[llm.local]`) instead of failing, even with Z.ai as the
  single provider. It logs one warning and skips Z.ai for 30 minutes, then
  tries Z.ai again. If the local model fails too, the Z.ai quota error is
  returned. Entering a new Z.ai key ends the cooldown.
- `GET /api/settings/master-agent` reports `active_provider`, `fallback_until`
  and `fallback_reason` while the cooldown is in effect.

### Changed

- With Z.ai as the single provider, only quota errors fail over to the local
  model; 5xx responses, timeouts and "not configured" are still returned as
  errors.
