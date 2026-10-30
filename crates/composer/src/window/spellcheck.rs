//! Spell checking the body as you type (PLAN-spellcheck).
//!
//! # Scheduling
//!
//! Every composer has one [`Checker`] (crate `spelling`) for its languages, its personal
//! dictionary and the words ignored in this draft. It is built on the background
//! executor (the first use of a language loads its dictionary, about 10 ms) and rebuilt
//! when one of those changes; dictionaries are shared by the whole process.
//!
//! An edit of the body bumps a version counter and (re)starts a
//! [`SPELLCHECK_DEBOUNCE`] timer. When it fires, the body's text goes to the background
//! executor, which splits it into blocks ([`spelling::blocks`]: paragraphs, with fenced
//! code kept together) and checks only the blocks whose text isn't in the cache of the
//! last check (about 0.2 ms per paragraph); the rest reuse their results, shifted to
//! where the block is now. A result comes back to the UI thread only if the version is
//! still the one it was started for (otherwise a newer check is already scheduled), and
//! replaces the underlines (a decoration collection of the editor: wavy, in the theme's
//! danger color; the ranges follow later edits until the next check replaces them).
//!
//! # Suggestions
//!
//! The right-click menu is built synchronously, but suggestions take tens of
//! milliseconds (German, release build). So after each check the suggestions for every
//! newly underlined word are computed one by one on the background executor and
//! cached by word and language; by the time someone right-clicks a word they are
//! there. A right click before that waits up to [`SUGGEST_WAIT`] for a computation on a
//! helper thread, and otherwise says the suggestions aren't ready.
//!
//! # Languages and the personal dictionary
//!
//! The app sets the languages of the `spellcheck` setting with [`set_spellcheck`]
//! (empty: off) and changes them live; the status item in the footer switches them for
//! one draft. The personal dictionary is loaded from the backend once per app
//! ([`ComposeBackend::spelling_words`](crate::ComposeBackend::spelling_words)) and kept
//! in a global; "Add to Dictionary" adds to it (every open composer re-checks) and
//! saves the word through the backend. "Ignore Spelling" only lasts for this draft.

use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::sync::Arc;
use std::time::Duration;

use gpui::{
    App, AppContext as _, BorrowAppContext as _, ClickEvent, Context, Focusable as _, Global,
    HighlightStyle, Hsla, SharedString, Subscription, Task, UnderlineStyle, Window, px,
};
use gpui_kit::component::{
    ActiveTheme as _, Theme,
    input::{TextDecoration, TextDecorationCollection},
    native_menu::NativeMenu,
};
use spelling::{Checker, Lang, Misspelling, PersonalWords};

use super::{Composer, MenuItem, Status};
use crate::actions::{AddToDictionary, IgnoreSpelling, ReplaceMisspelling, ToggleSpellingLanguage};

/// How long after the last edit of the body it is checked again.
pub const SPELLCHECK_DEBOUNCE: Duration = Duration::from_millis(150);

/// How long a right click waits for suggestions that weren't computed in advance.
const SUGGEST_WAIT: Duration = Duration::from_millis(150);

/// Suggestions are computed in advance for at most this many underlined words.
const MAX_PRECOMPUTED: usize = 30;

/// The languages the `spellcheck` setting resolved to (best first); empty: spell
/// checking is off. The app sets it ([`set_spellcheck`]); without it nothing is checked.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SpellcheckSettings {
    /// The languages, best first.
    pub languages: Vec<Lang>,
}

impl Global for SpellcheckSettings {}

/// Sets the languages every composer checks (empty: off). Open composers follow.
pub fn set_spellcheck(languages: Vec<Lang>, cx: &mut App) {
    let settings = SpellcheckSettings { languages };
    if cx.try_global::<SpellcheckSettings>() != Some(&settings) {
        cx.set_global(settings);
    }
}

/// The personal dictionary, shared by every composer of the app.
#[derive(Default)]
struct PersonalDictionary {
    words: Arc<PersonalWords>,
    /// Loaded from the backend (until then nothing is checked, so names don't flash).
    loaded: bool,
    /// Bumped on every change, to rebuild the checkers.
    generation: u64,
}

impl Global for PersonalDictionary {}

/// What a checker was built for.
#[derive(Debug, Clone, PartialEq, Eq)]
struct CheckerKey {
    langs: Vec<Lang>,
    personal: u64,
    ignored: usize,
}

/// The misspellings of one block, relative to its start.
type BlockCache = HashMap<String, Arc<Vec<Misspelling>>>;

/// A composer's spell checking state.
pub(super) struct Spelling {
    marks: TextDecorationCollection,
    /// The underline color the marks were set with (the theme's danger color).
    color: Hsla,
    /// Languages chosen for this draft in the status menu; `None`: the settings'.
    draft_langs: Option<Vec<Lang>>,
    /// Words ignored in this draft.
    ignored: Vec<String>,
    checker: Option<(CheckerKey, Arc<Checker>)>,
    building: Option<Task<()>>,
    /// Results of the last check per block text.
    cache: Arc<BlockCache>,
    /// Bumped on every edit of the body.
    version: u64,
    debounce: Option<Task<()>>,
    checking: Option<Task<()>>,
    /// The misspellings of the last check (as underlined; ranges as of that check).
    found: Vec<Misspelling>,
    /// Suggestions by word and language, for the right-click menu.
    suggestions: HashMap<(String, Option<Lang>), Vec<String>>,
    suggesting: Option<Task<()>>,
}

/// The body's right-click menu on an underlined word.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct SpellingMenu {
    range: Range<usize>,
    word: String,
    /// `None`: not ready yet.
    suggestions: Option<Vec<String>>,
}

impl SpellingMenu {
    /// Suggestions, Ignore and Add to Dictionary, then a separator.
    pub(super) fn items(&self) -> Vec<Option<MenuItem>> {
        let mut items = Vec::new();
        match &self.suggestions {
            Some(list) if !list.is_empty() => {
                for suggestion in list {
                    items.push(MenuItem::owned(
                        suggestion.clone().into(),
                        false,
                        Box::new(ReplaceMisspelling {
                            range: self.range.clone(),
                            word: self.word.clone(),
                            replacement: suggestion.clone(),
                        }),
                    ));
                }
            }
            Some(_) => items.push(MenuItem::new(
                "No Suggestions",
                true,
                Box::new(gpui::NoAction),
            )),
            None => items.push(MenuItem::new(
                "Suggestions Not Ready",
                true,
                Box::new(gpui::NoAction),
            )),
        }
        items.push(None);
        items.push(MenuItem::new(
            "Ignore Spelling",
            false,
            Box::new(IgnoreSpelling {
                word: self.word.clone(),
            }),
        ));
        items.push(MenuItem::new(
            "Add to Dictionary",
            false,
            Box::new(AddToDictionary {
                word: self.word.clone(),
            }),
        ));
        items.push(None);
        items
    }
}

/// "German (Austria)".
fn lang_name(lang: Lang) -> &'static str {
    match lang {
        Lang::DeDe => "German (Germany)",
        Lang::DeAt => "German (Austria)",
        Lang::DeCh => "German (Switzerland)",
        Lang::EnUs => "English (US)",
        Lang::EnGb => "English (UK)",
    }
}

fn underline(color: Hsla) -> HighlightStyle {
    HighlightStyle {
        underline: Some(UnderlineStyle {
            thickness: px(1.),
            color: Some(color),
            wavy: true,
        }),
        ..HighlightStyle::default()
    }
}

/// Check `text` block by block, reusing the results of blocks in `cache`.
fn check_blocks(
    checker: &Checker,
    text: &str,
    cache: &BlockCache,
) -> (Vec<Misspelling>, BlockCache) {
    let mut found = Vec::new();
    let mut next = BlockCache::new();
    for block in spelling::blocks(text) {
        let block_text = &text[block.clone()];
        let results = match next.get(block_text).or_else(|| cache.get(block_text)) {
            Some(results) => results.clone(),
            None => Arc::new(checker.check_text(block_text)),
        };
        found.extend(results.iter().map(|m| Misspelling {
            range: m.range.start + block.start..m.range.end + block.start,
            word: m.word.clone(),
            lang: m.lang,
        }));
        next.insert(block_text.to_owned(), results);
    }
    (found, next)
}

/// Suggestions on a helper thread, waiting at most [`SUGGEST_WAIT`].
fn suggest_now(checker: Arc<Checker>, misspelling: Misspelling) -> Option<Vec<String>> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::Builder::new()
        .name("spelling-suggest".to_owned())
        .spawn(move || {
            let _ = tx.send(checker.suggest(&misspelling));
        })
        .ok()?;
    rx.recv_timeout(SUGGEST_WAIT).ok()
}

impl Spelling {
    /// The state for `body`, and the subscriptions that keep it current.
    pub(super) fn new(
        body: &gpui::Entity<gpui_kit::component::input::EditorState>,
        backend: &Arc<dyn crate::ComposeBackend>,
        window: &mut Window,
        cx: &mut Context<'_, Composer>,
    ) -> (Self, Vec<Subscription>) {
        let marks = body.update(cx, |body, cx| {
            body.create_decorations_collection(Vec::new(), cx)
        });
        if !cx.has_global::<PersonalDictionary>() {
            cx.set_global(PersonalDictionary::default());
            // The backend reads the database: not on the UI thread.
            let loading = cx.background_spawn(backend.spelling_words());
            cx.spawn(async move |_, cx| {
                let words = match loading.await {
                    Ok(words) => words,
                    Err(err) => {
                        tracing::warn!(%err, "loading the personal spelling dictionary failed");
                        Vec::new()
                    }
                };
                cx.update_global::<PersonalDictionary, _>(|dictionary, _| {
                    let all = Arc::make_mut(&mut dictionary.words);
                    for word in &words {
                        all.insert(word);
                    }
                    dictionary.loaded = true;
                    dictionary.generation += 1;
                });
            })
            .detach();
        }
        let subscriptions = vec![
            cx.observe_global::<SpellcheckSettings>(|this, cx| this.run_spellcheck(cx)),
            cx.observe_global::<PersonalDictionary>(|this, cx| this.run_spellcheck(cx)),
            cx.observe_global_in::<Theme>(window, |this, _, cx| this.restyle_marks(cx)),
        ];
        let state = Self {
            marks,
            color: cx.theme().danger,
            draft_langs: None,
            ignored: Vec::new(),
            checker: None,
            building: None,
            cache: Arc::default(),
            version: 0,
            debounce: None,
            checking: None,
            found: Vec::new(),
            suggestions: HashMap::new(),
            suggesting: None,
        };
        (state, subscriptions)
    }
}

impl Spelling {
    /// Run the first check with `task` (the composer's initial check).
    pub(super) fn start(&mut self, task: Task<()>) {
        self.debounce = Some(task);
    }

    /// Whether the suggestions for `word` were computed in advance.
    #[cfg(test)]
    pub(super) fn has_suggestions_for(&self, word: &str) -> bool {
        self.suggestions.keys().any(|(w, _)| w == word)
    }

    /// How many blocks the last check kept results for.
    #[cfg(test)]
    pub(super) fn cache_blocks(&self) -> usize {
        self.cache.len()
    }
}

impl Composer {
    // ---- checking ---------------------------------------------------------------------

    /// The languages this draft is checked in (empty: not checked).
    pub fn spelling_languages(&self, cx: &App) -> Vec<Lang> {
        match &self.spelling.draft_langs {
            Some(langs) => langs.clone(),
            None => cx
                .try_global::<SpellcheckSettings>()
                .map(|s| s.languages.clone())
                .unwrap_or_default(),
        }
    }

    /// The underlines as the editor has them now: ranges and words.
    #[cfg(test)]
    pub(super) fn marked(&self, cx: &App) -> Vec<(Range<usize>, String)> {
        let text = self.body.read(cx).value();
        self.spelling
            .marks
            .get_ranges(cx)
            .into_iter()
            .map(|range| {
                let word = text.get(range.clone()).unwrap_or_default().to_owned();
                (range, word)
            })
            .collect()
    }

    /// The body changed: check it again once typing pauses.
    pub(super) fn schedule_spellcheck(&mut self, cx: &mut Context<'_, Self>) {
        self.spelling.version += 1;
        let timer = cx.background_executor().timer(SPELLCHECK_DEBOUNCE);
        self.spelling.debounce = Some(cx.spawn(async move |this, cx| {
            timer.await;
            let _ = this.update(cx, |this, cx| this.run_spellcheck(cx));
        }));
    }

    /// Check the body now (off the UI thread; the marks follow when it's done).
    pub(super) fn run_spellcheck(&mut self, cx: &mut Context<'_, Self>) {
        self.spelling.debounce = None;
        let langs = self.spelling_languages(cx);
        if langs.is_empty() || self.closed {
            self.spelling.checker = None;
            self.spelling.building = None;
            self.spelling.checking = None;
            self.spelling.cache = Arc::default();
            self.spelling.suggestions.clear();
            if !self.spelling.found.is_empty() {
                self.spelling.found.clear();
                cx.notify();
            }
            self.spelling.marks.clear(cx);
            return;
        }
        let Some(dictionary) = cx.try_global::<PersonalDictionary>().filter(|d| d.loaded) else {
            return;
        };
        let key = CheckerKey {
            langs,
            personal: dictionary.generation,
            ignored: self.spelling.ignored.len(),
        };
        let checker = match &self.spelling.checker {
            Some((built_for, checker)) if *built_for == key => checker.clone(),
            _ => {
                let mut words = (*dictionary.words).clone();
                for word in &self.spelling.ignored {
                    words.insert(word);
                }
                self.build_checker(key, words, cx);
                return;
            }
        };
        let text = self.body.read(cx).value();
        let cache = self.spelling.cache.clone();
        let version = self.spelling.version;
        let checking = cx
            .background_executor()
            .spawn(async move { check_blocks(&checker, &text, &cache) });
        self.spelling.checking = Some(cx.spawn(async move |this, cx| {
            let (found, cache) = checking.await;
            let _ = this.update(cx, |this, cx| {
                // Edited since: the newer check is scheduled already.
                if this.spelling.version == version {
                    this.spelling.cache = Arc::new(cache);
                    this.apply_spellcheck(found, cx);
                }
            });
        }));
    }

    fn build_checker(&mut self, key: CheckerKey, words: PersonalWords, cx: &mut Context<'_, Self>) {
        let langs = key.langs.clone();
        let building = cx
            .background_executor()
            .spawn(async move { Checker::new(&langs, words) });
        self.spelling.building = Some(cx.spawn(async move |this, cx| {
            let built = building.await;
            let _ = this.update(cx, |this, cx| {
                this.spelling.building = None;
                match built {
                    Ok(checker) => {
                        let same_langs = this
                            .spelling
                            .checker
                            .as_ref()
                            .is_some_and(|(old, _)| old.langs == key.langs);
                        if !same_langs {
                            this.spelling.suggestions.clear();
                        }
                        this.spelling.checker = Some((key, Arc::new(checker)));
                        this.spelling.cache = Arc::default();
                        this.run_spellcheck(cx);
                    }
                    Err(err) => tracing::error!(%err, "no spell checker"),
                }
            });
        }));
    }

    fn apply_spellcheck(&mut self, found: Vec<Misspelling>, cx: &mut Context<'_, Self>) {
        self.spelling.color = cx.theme().danger;
        let style = underline(self.spelling.color);
        self.spelling.marks.set(
            found
                .iter()
                .map(|m| TextDecoration::new(m.range.clone(), style))
                .collect(),
            cx,
        );
        if found != self.spelling.found {
            self.spelling.found = found;
            cx.notify();
        }
        self.precompute_suggestions(cx);
    }

    /// The theme changed: the underlines take its danger color.
    fn restyle_marks(&mut self, cx: &mut Context<'_, Self>) {
        let color = cx.theme().danger;
        if color == self.spelling.color {
            return;
        }
        self.spelling.color = color;
        let style = underline(color);
        let ranges = self.spelling.marks.get_ranges(cx);
        self.spelling.marks.set(
            ranges
                .into_iter()
                .map(|range| TextDecoration::new(range, style))
                .collect(),
            cx,
        );
    }

    /// Remove the underlines of the words `drop` picks (by range and text) right away,
    /// without waiting for the next check.
    fn unmark(&mut self, drop: impl Fn(&Range<usize>, &str) -> bool, cx: &mut Context<'_, Self>) {
        let text = self.body.read(cx).value();
        let style = underline(self.spelling.color);
        let kept: Vec<TextDecoration> = self
            .spelling
            .marks
            .get_ranges(cx)
            .into_iter()
            .filter(|range| !drop(range, text.get(range.clone()).unwrap_or_default()))
            .map(|range| TextDecoration::new(range, style))
            .collect();
        self.spelling.marks.set(kept, cx);
        let before = self.spelling.found.len();
        self.spelling.found.retain(|m| !drop(&m.range, &m.word));
        if self.spelling.found.len() != before {
            cx.notify();
        }
    }

    // ---- suggestions ------------------------------------------------------------------

    /// Compute the suggestions for newly underlined words in the background.
    fn precompute_suggestions(&mut self, cx: &mut Context<'_, Self>) {
        let Some((_, checker)) = self.spelling.checker.clone() else {
            return;
        };
        let wanted: HashSet<(String, Option<Lang>)> = self
            .spelling
            .found
            .iter()
            .map(|m| (m.word.clone(), m.lang))
            .collect();
        self.spelling
            .suggestions
            .retain(|key, _| wanted.contains(key));
        let mut pending: Vec<(String, Option<Lang>)> = Vec::new();
        for m in &self.spelling.found {
            let key = (m.word.clone(), m.lang);
            if !self.spelling.suggestions.contains_key(&key) && !pending.contains(&key) {
                pending.push(key);
            }
        }
        pending.truncate(MAX_PRECOMPUTED);
        if pending.is_empty() {
            return;
        }
        self.spelling.suggesting = Some(cx.spawn(async move |this, cx| {
            for (word, lang) in pending {
                let misspelling = Misspelling {
                    range: 0..word.len(),
                    word: word.clone(),
                    lang,
                };
                let for_checker = checker.clone();
                let list = cx
                    .background_executor()
                    .spawn(async move { for_checker.suggest(&misspelling) })
                    .await;
                let stored = this.update(cx, |this, _| {
                    let current = this
                        .spelling
                        .checker
                        .as_ref()
                        .is_some_and(|(_, c)| c.langs().eq(checker.langs()));
                    if current {
                        this.spelling.suggestions.insert((word, lang), list);
                    }
                    current
                });
                if !matches!(stored, Ok(true)) {
                    break;
                }
            }
        }));
    }

    /// The spelling part of the body's right-click menu: when the click (the cursor,
    /// which gpui-kit moved there) is on an underlined word.
    pub(super) fn spelling_menu(&mut self, cx: &mut Context<'_, Self>) -> Option<SpellingMenu> {
        let (cursor, selected, text) = {
            let body = self.body.read(cx);
            (body.cursor(), body.selected_range(), body.value())
        };
        let range = self
            .spelling
            .marks
            .get_ranges(cx)
            .into_iter()
            .find(|range| {
                if selected.is_empty() {
                    range.start <= cursor && cursor <= range.end
                } else {
                    range.start <= selected.start && selected.end <= range.end
                }
            })?;
        let word = text.get(range.clone())?.to_owned();
        let lang = self
            .spelling
            .found
            .iter()
            .find(|m| m.word == word)
            .and_then(|m| m.lang);
        let key = (word.clone(), lang);
        let suggestions = match self.spelling.suggestions.get(&key) {
            Some(list) => Some(list.clone()),
            None => {
                let checker = self.spelling.checker.as_ref().map(|(_, c)| c.clone())?;
                let misspelling = Misspelling {
                    range: 0..word.len(),
                    word: word.clone(),
                    lang,
                };
                let list = suggest_now(checker, misspelling);
                if let Some(list) = &list {
                    self.spelling.suggestions.insert(key, list.clone());
                }
                list
            }
        };
        Some(SpellingMenu {
            range,
            word,
            suggestions,
        })
    }

    // ---- actions ----------------------------------------------------------------------

    pub(super) fn replace_misspelling(
        &mut self,
        action: &ReplaceMisspelling,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let text = self.body.read(cx).value();
        if text.get(action.range.clone()) != Some(action.word.as_str()) {
            tracing::debug!("the misspelled word moved before its replacement was chosen");
            return;
        }
        let start = action.range.start;
        let replaced = start..start + action.replacement.len();
        self.body.update(cx, |body, cx| {
            body.set_selected_range(action.range.clone(), cx);
            body.replace(action.replacement.clone(), window, cx);
        });
        self.unmark(|range, _| *range == replaced, cx);
    }

    pub(super) fn ignore_spelling(&mut self, word: &str, cx: &mut Context<'_, Self>) {
        if !self.spelling.ignored.iter().any(|w| w == word) {
            self.spelling.ignored.push(word.to_owned());
        }
        self.unmark(|_, text| text == word, cx);
        self.run_spellcheck(cx);
    }

    pub(super) fn add_to_dictionary(&mut self, word: &str, cx: &mut Context<'_, Self>) {
        self.unmark(|_, text| text == word, cx);
        let added = cx.update_global::<PersonalDictionary, _>(|dictionary, _| {
            let added = Arc::make_mut(&mut dictionary.words).insert(word);
            if added {
                dictionary.generation += 1;
            }
            added
        });
        if !added {
            return;
        }
        let saving = cx.background_spawn(self.backend.add_spelling_word(word.to_owned()));
        let word = word.to_owned();
        cx.spawn(async move |this, cx| {
            if let Err(err) = saving.await {
                tracing::warn!(%err, "adding a word to the personal dictionary failed");
                let _ = this.update(cx, |this, cx| {
                    this.status = Status::Error(
                        format!("“{word}” couldn't be added to the dictionary: {err}").into(),
                    );
                    cx.notify();
                });
            }
        })
        .detach();
    }

    pub(super) fn toggle_spelling_language(&mut self, lang: Lang, cx: &mut Context<'_, Self>) {
        let mut langs = self.spelling_languages(cx);
        if let Some(ix) = langs.iter().position(|l| *l == lang) {
            langs.remove(ix);
        } else {
            langs.push(lang);
        }
        self.spelling.draft_langs = Some(langs);
        self.run_spellcheck(cx);
        cx.notify();
    }

    // ---- the status item --------------------------------------------------------------

    /// What the status item in the footer says; `None` while the `spellcheck` setting
    /// is off.
    pub fn spelling_status(&self, cx: &App) -> Option<SharedString> {
        let settings_on = cx
            .try_global::<SpellcheckSettings>()
            .is_some_and(|s| !s.languages.is_empty());
        if !settings_on {
            return None;
        }
        let langs = self.spelling_languages(cx);
        if langs.is_empty() {
            return Some("Spelling off".into());
        }
        let tags = langs
            .iter()
            .map(|l| l.as_str())
            .collect::<Vec<_>>()
            .join(" · ");
        Some(
            match self.spelling.found.len() {
                0 => format!("✓ Spelling {tags}"),
                1 => format!("1 spelling issue · {tags}"),
                n => format!("{n} spelling issues · {tags}"),
            }
            .into(),
        )
    }

    /// The status item's menu: the languages to check this draft in.
    pub(super) fn show_spelling_menu(
        &mut self,
        event: &ClickEvent,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        // The menu's actions go to the focused element: make sure that is in here.
        if !self.focus.contains_focused(window, cx) {
            let body = self.body.read(cx).focus_handle(cx);
            window.focus(&body, cx);
        }
        let langs = self.spelling_languages(cx);
        Lang::ALL
            .into_iter()
            .fold(NativeMenu::new(), |menu, lang| {
                menu.menu_with_check(
                    format!("{} — {lang}", lang_name(lang)),
                    langs.contains(&lang),
                    Box::new(ToggleSpellingLanguage { lang }),
                )
            })
            .show(event.position(), window, cx);
    }
}
