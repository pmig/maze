# Agents in Maze

Maze runs coding agents such as Claude Code, Codex, Gemini CLI, Cursor's agent and pi
in its fourth column, the way Zed runs external agents: over the
[Agent Client Protocol](https://agentclientprotocol.com) (ACP). The agent works, you
review. This page covers picking and installing agents, signing in, what the agent can
and can't see, the pi caveat, and adding your own agents.

The design is in [ADR-006](adr/0006-agent-interface.md),
[ADR-013](adr/0013-agent-sandbox.md) and [ADR-016](adr/0016-cursor-and-pi-agents.md).

## Turn on AI for an account

AI is off for every account until you turn it on. In `settings.json`:

```jsonc
{
  "agent": {
    "accounts": { "Work": "any" }   // "off" (default), "local_only" or "any"
  }
}
```

Agents that Maze starts only ever see accounts set to `"any"`. Accounts set to `"off"`
or `"local_only"` are never in an agent session's token, and a thread of such an
account is never attached to a conversation. Changing the setting takes effect at once,
even for running sessions.

## Pick and install an agent

Open the agent column (`cmd-?`, or "Agent: Toggle Agent Panel" in the command palette).
The picker lists:

| Agent        | Registry id  | How Maze installs it                                                    |
| ------------ | ------------ | ----------------------------------------------------------------------- |
| Claude Code  | `claude-acp` | npm package `@agentclientprotocol/claude-agent-acp`                     |
| Codex        | `codex-acp`  | npm package `@agentclientprotocol/codex-acp`                            |
| Gemini CLI   | `gemini`     | npm package `@google/gemini-cli` (`--acp`)                              |
| Cursor       | `cursor`     | Cursor's `cursor-agent` binary, downloaded from Cursor when you install |
| pi           | `pi-acp`     | npm packages `pi-acp` and `@earendil-works/pi-coding-agent`             |

"More agents from the registry" shows the rest of the
[ACP registry](https://cdn.agentclientprotocol.com/registry/v1/latest/registry.json).
Maze caches the registry in `<data dir>/agents/registry.json` and uses the copy when
you're offline.

**Install** puts the agent into `<data dir>/agents/<id>/<version>/`, once per
version; Maze starts it from there and never fetches it on launch. **Update** appears
when the registry has a newer version.

- npm agents run on Maze's own Node runtime (Node 24, in `<data dir>/runtimes/node/`),
  downloaded from nodejs.org on the first install and checked against digests built
  into Maze. Without a network connection Maze falls back to a `node` 22 or newer on
  your `PATH`. npm itself runs in the agent sandbox, because install scripts are code
  from the internet.
- Binary agents are downloaded over HTTPS and checked against the registry's SHA-256
  when it lists one (Cursor's entry doesn't). `.tar.gz` archives and plain binaries are
  supported; `.zip` and `uvx` agents aren't yet.

Cursor's agent is proprietary: Maze downloads it from Cursor's own URL when you install
it and never redistributes it. A Cursor subscription pays for the agent; it doesn't
become a model provider for anything else in Maze.

The `agent.default` setting picks the agent selected in the picker (`"claude"` by
default; short names `claude`, `codex`, `gemini`, `cursor` and `pi` work).

## Conversations

**Start** opens a conversation. The thread selected in the message list is attached as
context: Maze writes it as Markdown to `thread.md` in the session's folder and tells the
agent about it with your first prompt. Select another thread and click **Attach selected
thread** to switch.

- `enter` sends, `shift-enter` starts a new line, the square button stops the agent.
- Answers stream in as Markdown. Tool calls show collapsed with their status; click one
  to see its details. Plans show as a checklist.
- `cmd-n` (or **+**) starts a new conversation. The clock button lists the
  conversations about the selected thread; click one to continue it. Conversations are
  stored encrypted in Maze's database; agents that support `session/load` also get
  their earlier session back.

### Permission prompts

When the agent wants to use one of its own tools, or run a command that isn't a plain
`mazemail …` call, Maze asks inline:

- **Allow once**: this call only.
- **Allow for this session**: for tools, the agent's "always allow" for the rest of the
  session; for commands, that program (for example `git`) for the rest of the session.
- **Deny**.

A single, plain `mazemail …` command (no pipes, `;`, `$(…)` or redirections) runs
without asking, in the session folder. Everything still runs in the sandbox, approved
or not.

### Proposals

Under the default `triage` profile, `mazemail archive|move|tag|flag|read` and
`mazemail propose --file` don't change mail; they become proposals that show at the top
of the agent column, grouped, with **✓** and **✗** per item. Click an item to give the
review focus, then `y` accepts and `n` rejects the highlighted item (`up`/`down` move).
`cmd-shift-enter` accepts everything, `cmd-shift-backspace` rejects everything.
Accepted items are applied like your own commands and are listed in the audit log under
the agent's token.

`agent.session_profile` sets what an agent session's token may do: `"read-only"`,
`"triage"` (default) or `"writer"` (changes apply directly).

## Signing in

Each agent keeps its own login in its own home, `<data dir>/agent-home/<id>/`, separate
from the copy you use in a terminal. When an agent needs a login, the column shows its
sign-in methods:

- Methods the agent handles itself (for example "Log in with ChatGPT") run when you
  click them. Agents in the sandbox can't open your browser; if the agent prints a
  sign-in link, Maze shows it so you can open it yourself.
- Terminal logins show the command to run in a terminal (with `HOME` pointing at the
  agent's home in Maze). Run it, then click the method again.

Agents that read API keys from the environment don't see yours: Maze starts agents with
an empty environment plus `PATH`, `HOME`, `TMPDIR`, `LANG`, `SHELL` and the variables
below. To give an agent a key, add it under `agent.env` (stored in plain text in your
settings file):

```jsonc
{ "agent": { "env": { "codex-acp": { "OPENAI_API_KEY": "sk-…" } } } }
```

## What an agent sees

Every agent process Maze starts, everything it starts, and every command Maze runs for
it runs in an operating system sandbox (Seatbelt on macOS, bubblewrap on Linux).

| Access   | Policy                                                                                                                                                                                                                                                                        |
| -------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Read     | System locations; inside your home directory only the agent's own home, this session's folder, the installed agents, the runtimes, the `mazemail` CLI and, on macOS, `~/Library/Keychains` (agents keep their logins in the Keychain; items stay protected by their access lists). Not your documents, `~/.ssh`, the Thunderbird profile, Apple Mail, other sessions, and never Maze's `mail/` folder |
| Write    | This session's folder, its temporary folder and the agent's own home. Not your real `~/.claude`, `~/.codex` or other dotfiles                                                                                                                                                 |
| Network  | Outbound to any host (agents have to reach their model provider)                                                                                                                                                                                                              |
| Maze API | Only `mazemail.sock`; no other Unix socket (Docker, ssh-agent, password managers)                                                                                                                                                                                              |

Mail reaches the agent only through the `mazemail` CLI with the session's token
(`MAZEMAIL_SESSION`), limited to the accounts you shared and the session profile, and
revoked when the session ends. The session folder, `<data dir>/sessions/<id>/`, holds
`AGENTS.md` (what `mazemail` can do, generated from its command registry), `CLAUDE.md`
and `GEMINI.md` pointing at it, and `thread.md` when a thread is attached. It is
plaintext, because the agent has to read it, holds only what you gave that session,
and is deleted when the session ends (leftovers at the next start). The shield in the
column's header shows the sandbox state; hover it for the effective policy.

Settings (`agent_sandbox`):

```jsonc
{
  "agent_sandbox": {
    "enabled": true,           // false: agents run unsandboxed (a warning stays in the column)
    "allow_read": [],          // extra paths in your home agents may read, e.g. "~/Documents/contracts"
    "deny_read": [],           // extra paths agents may never read
    "agent_paths": {           // per agent: extra paths, and agents that sandbox themselves
      "claude-acp": { "read": [], "write": [], "already_sandboxed": false }
    },
    "network_hosts": null      // null: any host; []: no network
  }
}
```

`already_sandboxed: true` runs that agent's own process outside Maze's sandbox, for
agents that sandbox themselves and fail inside Maze's (Seatbelt can't be nested); the
commands it asks Maze to run stay sandboxed. Limiting agents to certain hosts
(`network_hosts` with entries) isn't implemented yet; Maze refuses to start an agent
with such a setting rather than ignore it.

**Linux** gets the bubblewrap mounts (your home is replaced by an empty folder with
only the allowed paths bound back, `/tmp` and `$XDG_RUNTIME_DIR` hidden). The seccomp
filter that blocks new Unix sockets and the loopback bridge for the API aren't
implemented yet, so abstract-namespace sockets (such as some D-Bus buses) stay
reachable while the network is shared. Without `bwrap`, agents don't start; the error
explains how to install it or turn the sandbox off.

## pi

pi runs its own shell and file tools without asking and not through Maze's terminal,
so there are no permission prompts: **the sandbox is the boundary**. The column says so
when pi is selected. pi keeps its settings, provider logins and sessions in `~/.pi`,
which inside Maze is pi's agent home, so pi in Maze is configured separately from pi in
a terminal. Sign in to your model provider from pi's own commands.

## Custom agents

Any ACP agent can be added in settings:

```jsonc
{
  "agent": {
    "custom": {
      "my-agent": {
        "name": "My agent",
        "command": "/usr/local/bin/my-agent",   // or a name on PATH
        "args": ["--acp"],
        "env": { "MY_AGENT_MODEL": "small" }
      }
    }
  }
}
```

Custom agents appear in the picker after the built-in ones and run in the same sandbox
(their program's folder is readable). Their environment is Maze's minimal one plus
`env`.

## Agents outside Maze

Agents you start yourself (Claude Code in a terminal, Zed, Cursor) aren't sandboxed by
Maze. They reach mail with a token you approve through `mazemail login`; see
[cli.md](cli.md) and [security.md](security.md).
