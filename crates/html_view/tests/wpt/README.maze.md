# Web Platform Tests for the original view

A selection of [Web Platform Tests](https://github.com/web-platform-tests/wpt)
reftests (BSD-3-Clause, `LICENSE.md`), the test suite browsers share. A reftest is a
test page and a reference page built differently that must look the same (or, for
`mismatch`, different). `src/wpt.rs` renders both through what a message goes
through in the original view: Maze's sanitizer, the embedding rewrite (resources,
presentational hints, table fixes; `mazemail_mime::html::hints`), the document and
Blitz, at 800×600. So the run checks the engine and Maze's own HTML passes against the
specs, not only against the mail fixtures.

- **Commit:** `9986bb32c9e97707eff870ab284ebc5cc7ae8d21` (2026-11-21), the one Blitz's
  own WPT runner pins (`wpt/WPT_COMMIT` in DioxusLabs/blitz), so results compare.
- **Directories** (what mail uses): `html/rendering/non-replaced-elements/{tables,
  flow-content-0, phrasing-content-0, the-hr-element-0, lists, the-br-element}`,
  `html/rendering/the-css-user-agent-style-sheet-and-presentational-hints`,
  `html/rendering/replaced-elements/images`, `css/css-tables`, `css/CSS2/tables`,
  with the references they use from elsewhere (`css/reference`, …).
- **Kept** are reftests that a message could be: no script (also no `reftest-wait`),
  no web fonts (Ahem), no external style sheets, no form controls, frames, SVG or
  media, no vertical writing modes, no printing; XHTML only where it parses the
  same as HTML. Local images load like a message's remote images.
- **`expectations.txt`** lists the tests that fail today, each with a note on what it's
  waiting for: a Blitz issue or pull request, "fixed on Blitz main" (not released
  yet), "no Blitz issue" or a cause in Maze itself (the sanitizer drops attributes of
  `<body>`, `<html>`, `li type` and `br clear`; documents always render in standards
  mode). The run fails when a test
  starts failing and when a listed one starts passing; after a change that fixes or
  breaks tests on purpose, regenerate it:

  ```sh
  MAZEMAIL_WPT_UPDATE=1 cargo nextest run -p html_view -E 'test(wpt_reftests)'
  ```

To update to another WPT commit: sparse-check out the directories above plus
`css/reference`, `css/CSS2/reference`, `css/support` and `common` into an empty
directory, then run `python3 -I select.py <checkout> <this directory> <directories…>`
and regenerate the expectations.
