# ADR-028: Maze Magic, one-shot agent sessions

- **Status:** Accepted
- **Date:** 2026-11-18

## Context

People want an agent to look over a reply before it goes out: fix typos, keep the tone
polite and check it against the conversation (an unanswered question, a forgotten
attachment, a wrong date). The agent column (ADR-006, ADR-016) can already do this,
but only as a conversation: select the thread, describe the task, copy the result back
into the draft. That is too slow for something done before every important reply.

An agent session in the column is built for longer work. It is stored as an agent
thread with a transcript, gets a session token for the `mazemail` CLI, writes the
attached thread to its session folder and asks the user about tools and commands. None
of that fits a single "improve this text" request, and the token would give the agent
more mail than the one draft and its conversation.

## Decision

`cmd-m` in the composer (Maze Magic) runs the configured agent (`maze_magic.agent`,
else `agent.default`) in an **ephemeral session** for one prompt. The prompt holds the
user's instructions (`maze_magic.prompt`, with a default), the draft's own text, the
quote below it and, with `maze_magic.include_thread`, the conversation as the agent
export renders it. The answer comes back between `<maze-draft>` markers (or as the
whole answer) and is shown in a dialog over the draft: Accept & Send (the default),
Accept & Edit or Discard.

An ephemeral session (`SessionConfig::ephemeral`, `AgentManager::run_magic`):

- gets no session token and no `mazemail` on its `PATH`, so it reaches no mail;
- denies every permission request and runs no terminal command;
- stores no agent conversation and no transcript, and writes no `thread.md`;
- runs in the same sandbox as other sessions, and ends when the answer is complete,
  after three minutes, or when the dialog closes.

Only the text above the quote
(`quote_start`) is replaced; the quote stays as it was.

## Consequences

- Maze Magic needs no new trust: the agent and its sign-in are the agent column's,
  and it sees less than a column session does (only what is in the prompt).
- Starting an agent takes a few seconds, so the dialog shows that it is working and
  can be cancelled.
- Agents that ignore the markers still work (the whole answer is taken), and the user
  always reviews the text before anything is sent.
- `cmd-m` means Maze Magic inside a composer and Minimize everywhere else. On macOS
  the Window menu's key equivalent may see `cmd-m` before the composer does; if it
  does, Minimize's menu key has to change.
- A diff view of the changes, and choosing among several prompts, can come later.
