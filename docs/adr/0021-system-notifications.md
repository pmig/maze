# ADR-021: System notifications for new mail

- **Status:** Accepted
- **Date:** 2026-10-29

## Context

New mail should show a native notification: Notification Center on macOS, the
desktop's notification service on Linux. A click should bring Maze to the front with
the message open. The `notifications` settings (`enabled`, `content`) were already
defined (CONCEPT §16). Maze is GPL-3.0-or-later and `deny.toml` allows permissive
licenses only (ADR-001).

The Rust options in October 2026:

- **notify-rust** (MIT/Apache, about 1.4k stars, 483 dependent crates, active). On
  Linux it speaks the freedesktop D-Bus protocol, async over zbus. On macOS it uses
  `mac-notification-sys`, built on `NSUserNotification`, which Apple deprecated in
  macOS 11 and which shows no banner while the app is in front. Its
  `UNUserNotificationCenter` backend (`preview-macos-un`, since 4.18.0) is labelled a
  preview and only waits for clicks by blocking a thread; the async API is planned for
  5.0 (issue #277).
- **mac-usernotifications** (MIT/Apache), by the notify-rust maintainer, is the
  `UNUserNotificationCenter` wrapper behind that backend, with an async API, a delegate
  that presents banners in the foreground, and authorization requests.
- **user-notify** (LGPL-3.0-or-later): the modern macOS API on all platforms, but one
  maintainer, two releases on one day and no dependent crates; LGPL would need a
  `deny.toml` exception.
- Own `objc2-user-notifications` bindings: permissive, but code we'd maintain and
  can't test without a Mac.

## Decision

A new L1 crate, `notifications`, shows notices and reports clicks, with no GPUI:

- **macOS:** `mac-usernotifications` (`UNUserNotificationCenter`). It needs the signed
  `Maze.app` bundle and the user's permission, asked for before the first
  notification. Without a bundle (`cargo run`) notifications are off.
- **Linux and the BSDs:** `notify-rust` on zbus with Tokio, with Maze's desktop entry
  and the `email.arrived` category; the body is escaped when the server renders
  markup. The icon is Maze's logo, embedded in the crate and written once to
  `<data dir>/icons/<bundle id>.png`, because dev builds and unpacked tarballs have no
  icon theme entry. On macOS Notification Center shows the icon of `Maze.app`.

The core decides what counts as new mail (`mazemail_core::new_mail`): added to an
Inbox, unread, not from one of the account's identities, and received by the server
after the account's sync started in this session (less five minutes for clock skew).
That keeps first syncs, UIDVALIDITY resets and mail moved back into the Inbox quiet
without tracking sync passes. The GUI gathers what arrives within 0.8 s and shows one
notification per message, or one summary for more than three.

## Consequences

- No new license in `deny.toml`; both crates come from notify-rust's maintainer.
- When notify-rust 5.0 makes `UNUserNotificationCenter` its default with an async API,
  the macOS backend can move to notify-rust and the crate keeps its interface.
- Mail that arrived while Maze wasn't running isn't notified.
- The macOS backend can be checked from Linux
  (`cargo clippy -p notifications --target aarch64-apple-darwin`), but it must be tried
  in a signed build before a release; development builds without a bundle show
  nothing. `mise start` runs dev builds from a minimal `Maze Dev.app`
  (`script/dev-bundle`), so they show notifications under the dev bundle ID.
- The `notifications.dock_badge` setting is separate and not implemented yet.
