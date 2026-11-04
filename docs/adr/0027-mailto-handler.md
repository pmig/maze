# ADR-027: Maze as the system's mailto: handler

- **Status:** Accepted
- **Date:** 2026-11-03

## Context

Clicking a `mailto:` link in a browser or another app should open a new message in
Maze. macOS hands the link to the running app (or starts it) through an open-URL event;
`Info.plist` already declared the scheme. Linux and Windows start the program with the
link as its argument (`Exec=mazemail %u`), also when Maze is already running. Maze
holds a lock on its data directory, so a second process can't open the mail itself:
it has to hand the link to the running one.

The running Maze already serves the local API on a private Unix socket
(ADR-006). Every request that touches mail needs a token; `help`, `status` and
`login` work without one.

## Decision

`mazemail <mailto:…>` asks the running Maze to open the link with a new local API
command, `open`, and starts Maze with the link if none is running. macOS's open-URL
events go the same way inside the app. The link becomes a new draft with its
recipients, subject and body, shown for the user to review and send.

`open` works without a token. It accepts only `mailto:` links, reads no mail and sends
nothing; without a window (`--headless`) it fails. `mazemail open` is also a CLI
command, for scripts.

## Consequences

- One mechanism for the second process and for scripts; no extra socket, and the link
  is never written to disk (its body is text from elsewhere, possibly private).
- Any program running as the user can put a prefilled message in front of them, as a
  `mailto:` link on a web page can. It can't read or send mail through `open`.
- The tokenless command list (tested) grows by one; the security docs say why.
- Becoming the default handler stays the user's choice: Apple Mail's settings on
  macOS, `xdg-mime default dev.mazemail.maze.desktop x-scheme-handler/mailto` on Linux.
