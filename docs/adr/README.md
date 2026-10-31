# Architecture decision records

Each ADR records one decision, why we took it, and what follows from it. ADR-001 to
ADR-014 come from the decisions table in PLAN.md §1. They are working decisions: the
M0 spikes (PLAN §6) can overturn them, and a changed decision gets a new ADR that
supersedes the old one instead of an edit.

New ADRs copy [`template.md`](template.md), take the next free number, and are added
to the index below.

| ADR                                          | Title                          | Status                      |
| -------------------------------------------- | ------------------------------ | --------------------------- |
| [001](0001-license.md)                       | License                        | Accepted (working decision) |
| [002](0002-ui-stack.md)                      | UI stack                       | Accepted (working decision) |
| [003](0003-imap-library.md)                  | IMAP library                   | Accepted (working decision) |
| [004](0004-html-original-mode.md)            | HTML original mode             | Superseded by ADR-020       |
| [005](0005-storage.md)                       | Storage                        | Accepted (working decision) |
| [006](0006-agent-interface.md)               | Agent interface                | Accepted (working decision) |
| [007](0007-gmail-sync.md)                    | Gmail sync                     | Accepted (working decision) |
| [008](0008-compose-editor.md)                | Compose editor                 | Accepted (working decision) |
| [009](0009-toolchain.md)                     | Toolchain                      | Accepted (working decision) |
| [010](0010-auto-update.md)                   | Auto-update from day one       | Accepted (working decision) |
| [011](0011-release-process.md)               | Release process                | Accepted (working decision) |
| [012](0012-encryption-at-rest.md)            | Encryption at rest             | Accepted (working decision) |
| [013](0013-agent-sandbox.md)                 | Agent sandbox                  | Accepted (working decision) |
| [014](0014-name.md)                          | Name                           | Accepted (working decision) |
| [015](0015-crate-naming-collisions.md)       | Crate naming collisions        | Accepted (working decision) |
| [016](0016-cursor-and-pi-agents.md)           | Cursor and pi as ACP agents    | Accepted (working decision) |
| [017](0017-account-secrets-in-the-vault.md)  | Account secrets in the vault   | Accepted                    |
| [018](0018-workspace-panes.md)               | Workspace panes, tabs, splits  | Accepted                    |
| [019](0019-imap-only.md)                     | IMAP only, no Local Folders    | Accepted                    |
| [020](0020-servo-for-original-html.md)       | Servo for original HTML        | Accepted                    |
| [021](0021-system-notifications.md)          | System notifications           | Accepted                    |
| [022](0022-go-to-folder-and-search-keys.md) | Go to Folder, search keys, Ctrl | Accepted                   |
| [023](0023-spell-checking.md)               | Spell checking in the composer | Accepted                    |
| [024](0024-server-owned-contacts.md)        | Server-owned contacts          | Accepted                    |
| [025](0025-update-manifests-on-cloudflare.md) | Update manifests on a Cloudflare Worker | Accepted              |
