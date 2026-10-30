# ADR-023: Spell checking in the composer

- **Status:** Accepted
- **Date:** 2026-10-30

## Context

The composer needs spell checking as you type (PLAN §4.10, PLAN-spellcheck), first for
German (Germany, Austria, Switzerland) and English, the languages Maze's first users
write in, often both in one mail. Zed has no built-in checker to copy; the closest
model is Codebook (LSP): `spellbook` with Hunspell dictionaries, wavy underlines,
suggestions and "Add to dictionary". The body is gpui-kit's `EditorState`, which can
decorate text ranges (ADR-008, step 2 of PLAN-spellcheck). Checking a paragraph takes
about 0.2 ms, but loading a dictionary takes about 10 ms and German suggestions up to
100 ms (release build), so none of it may run on the UI thread. Mail text and the
words people add (often names) are private: nothing may leave the computer or be
written in plaintext.

## Decision

- **Engine and data:** the L1 crate `spelling` (no GPUI, no IO) with `spellbook` and
  the bundled `wooorm/dictionaries` lists for de-DE, de-AT, de-CH, en-US and en-GB
  (licenses in `third_party/dictionaries`), on every platform; no NSSpellChecker for
  now.
- **Languages:** each paragraph is checked in its detected language (≥ 20 letters),
  and by the **mixed-language rule** also accepts the words of the other enabled
  languages: a word is a misspelling only if every enabled dictionary rejects it
  (`spelling::ACCEPT_OTHER_LANGUAGES`). Suggestions come from the detected language;
  the other languages add theirs only when they are closer to the word.
- **Scheduling:** one `Checker` per composer, built on GPUI's background executor.
  Edits restart a 150 ms debounce; the check runs in the background, re-checks only the
  blocks (`spelling::blocks`: paragraphs, fenced code kept whole) whose text changed,
  and is dropped if the body changed meanwhile (a version counter). Underlines are an
  editor decoration collection, so they follow edits until the next check.
- **Suggestions** are computed in the background for every underlined word after each
  check and cached, because the right-click menu is built synchronously; a menu
  opened before they are ready waits at most 150 ms on a helper thread.
- **Personal dictionary:** one list for all languages in the encrypted database
  (`spelling_word`, schema v12), read and written through `mazemail_core` and the
  composer's `ComposeBackend`, cached in a GPUI global so every open composer re-checks
  when a word is added. "Ignore" lasts for the draft only.
- **Setting:** `spellcheck` is a list of tags, `[]` (or `true`) for the system's
  preferred languages that have a dictionary (`sys-locale`; en-US if none), or `false`.
  Unknown tags are warnings in the settings editor. `settings` depends on `spelling`
  (same layer, pure) to know the tags.

## Consequences

- German and English mail is checked offline without platform differences; other
  languages need another dictionary in `third_party/` and a `Lang` variant.
- The mixed-language rule misses a typo that happens to be a word in another enabled
  language ("und" for "and" in English text with German on). Accepted: flagging every
  English word in German mail was the bigger problem.
- Words added by mistake can't be removed in the app yet (the core has
  `remove_spelling_word`); a dictionary editor comes with the settings window.
- The subject line isn't checked: single-line inputs have no decorations in gpui-kit.
