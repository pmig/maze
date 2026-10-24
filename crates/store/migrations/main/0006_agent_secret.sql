-- Maze store, schema v6: agent sign-in secrets in the encrypted database (ADR-017).
--
-- An API key or token the user pastes into the agent column's sign-in card (an
-- Anthropic API key, a `claude setup-token` token, an OpenAI or Gemini key, …). Maze
-- injects it into that agent's environment as `env` when it starts the agent; it is
-- never written to settings.json, the agent's home or a log. One per agent (keyed by
-- the agent's registry id or custom id); "Remove key" deletes the row.

CREATE TABLE agent_secret (
  agent_id TEXT PRIMARY KEY,
  env      TEXT NOT NULL,
  secret   TEXT NOT NULL,
  updated  INTEGER NOT NULL
) WITHOUT ROWID;
