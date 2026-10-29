# ADR-022: `cmd-k` goes to a folder, search moves to `cmd-shift-f`, `cmd` is Ctrl on Linux

- **Status:** Accepted
- **Date:** 2026-10-29

## Context

The keymap's rule is that Thunderbird's bindings win every conflict (CONCEPT §12), so
`cmd-k` searched all messages and `cmd-shift-k` focused the Quick Filter, and ADR-018
kept `cmd-k` free of Zed's `cmd-k <arrow>` chords for that reason.

The keymap writes the main modifier as `cmd`, which GPUI reads as the Super key on
Linux, so Linux users had Super+K and so on, where Ctrl is the convention and Super
often belongs to the window manager.

Opening a folder needed the mouse or the folder pane's tree. Zed, Slack and Linear
use `cmd-k` for a quick switcher, and Quick Move (`shift-m`) already has a fast picker
over folders. In Zed, `cmd-f` searches the open file and `cmd-shift-f` the whole
project, which maps onto the Quick Filter (the open folder) and search (every
account).

## Decision

- `cmd-k` opens **Go to Folder**, a picker over the unified folders, the favorites and
  every account's folders, ranked like Quick Move, that opens the picked folder.
- `cmd-shift-f` and `cmd-/` focus **search all messages**.
- `cmd-f` focuses the **Quick Filter**; Thunderbird's `cmd-shift-k` keeps working.

This is a deliberate exception to "Thunderbird wins" for `cmd-k`. Zed's `cmd-k <arrow>`
chords stay unbound (ADR-018), now so that Go to Folder opens at once.

There is still one keymap, written with `cmd`. When Maze binds it, `cmd` becomes the
primary modifier from the `keymap.primary_modifier` setting: `"platform"` (the
default: Command on macOS, Ctrl on Linux), `"ctrl"` or `"super"` on any platform. A
keystroke with both `cmd` and `ctrl` keeps two modifiers (`ctrl` turns into Super). A
test keeps the default keymap free of clashes either way. Labels in the UI come from
the same translation (`ui::keymap::shortcut_label`).

## Consequences

- Thunderbird users press `cmd-shift-f` or `cmd-/` instead of `cmd-k` to search; the
  getting-started guide and the shortcuts table say so.
- `cmd-f` is taken in the window; a future "find in this message" needs a context of
  its own (the message view) to bind it there.
- GPUI can't remove single bindings, so a changed `primary_modifier` applies after a
  restart.
- Tests bind the keymap as written (`cmd`), on every platform.
- A user `keymap.json` (not read yet) will go through the same translation.
