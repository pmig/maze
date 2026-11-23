//! Web Platform Tests reftests through the original view (`tests/wpt/README.maze.md`).
//!
//! Each test and its references go through what a message goes through (the
//! sanitizer, the embedding rewrite with its presentational hints, the document, Blitz)
//! and render at 800×600, WPT's size. A test passes when it looks like its `match`
//! references and unlike its `mismatch` ones, within its `<meta name=fuzzy>`
//! tolerance. `tests/wpt/expectations.txt` lists the tests that fail today: the run
//! fails on a listed test that passes (take it off the list) as much as on one that
//! fails without being listed. `MAZEMAIL_WPT_UPDATE=1` rewrites the list, and
//! `MAZEMAIL_WPT_OUT=<dir>` keeps the renderings of failing tests.

// Test code only (`cfg(test)` in lib.rs); tests may unwrap (AGENTS.md).
#![allow(clippy::unwrap_used)]

use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use crate::document::{DocumentOptions, OriginalDocument};
use crate::engine::{Inbox, Page};
use crate::view::{Fetch, Part, PartSource};

/// Images of the tests are served from here (rewritten from their relative paths).
const IMAGE_ORIGIN: &str = "https://wpt.example/";

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/wpt")
}

/// A reftest: the test file and its references, paths relative to [`root`].
struct Reftest {
    path: String,
    references: Vec<(Relation, String)>,
    fuzzy: Fuzzy,
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Relation {
    Match,
    Mismatch,
}

/// The tolerance of a comparison: the most a channel may differ and how many pixels
/// may differ at all.
#[derive(Clone, Copy, Debug)]
struct Fuzzy {
    max_difference: u8,
    total_pixels: usize,
}

impl Fuzzy {
    const EXACT: Self = Self {
        max_difference: 0,
        total_pixels: 0,
    };

    /// `maxDifference=0-2;totalPixels=0-40`, also `2;40` and with a `<ref>:` prefix
    /// (applied to every reference). Upper bounds only.
    fn parse(content: &str) -> Self {
        let content = content
            .rsplit_once(':')
            .filter(|(before, _)| !before.contains('='))
            .map_or(content, |(_, after)| after);
        let mut values = content.split(';').map(|part| {
            let range = part.rsplit('=').next().unwrap_or(part).trim();
            let upper = range.rsplit('-').next().unwrap_or(range).trim();
            upper.parse::<usize>().unwrap_or(0)
        });
        let max_difference = values.next().unwrap_or(0);
        let total_pixels = values.next().unwrap_or(0);
        Self {
            max_difference: u8::try_from(max_difference).unwrap_or(u8::MAX),
            total_pixels,
        }
    }
}

/// The attribute `name` of a tag's source text (`<link rel=match href="…">`).
fn attribute<'a>(tag: &'a str, name: &str) -> Option<&'a str> {
    let lower = tag.to_ascii_lowercase();
    let mut from = 0;
    while let Some(found) = lower[from..].find(name) {
        let at = from + found;
        from = at + name.len();
        let before = lower[..at].chars().last();
        if !before.is_some_and(char::is_whitespace) {
            continue;
        }
        let rest = tag[from..].trim_start();
        let Some(rest) = rest.strip_prefix('=') else {
            continue;
        };
        let rest = rest.trim_start();
        return Some(match rest.chars().next() {
            Some(quote @ ('"' | '\'')) => rest[1..].split(quote).next().unwrap_or(""),
            _ => rest
                .split(|c: char| c.is_whitespace() || c == '>')
                .next()
                .unwrap_or(""),
        });
    }
    None
}

/// The tags named `name` in `html`, as source text.
fn tags<'a>(html: &'a str, name: &str) -> Vec<&'a str> {
    let lower = html.to_ascii_lowercase();
    let open = format!("<{name}");
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(found) = lower[from..].find(&open) {
        let start = from + found;
        let end = lower[start..]
            .find('>')
            .map_or(html.len(), |end| start + end + 1);
        out.push(&html[start..end]);
        from = end;
    }
    out
}

/// `href` resolved against the file at `base` (both relative to [`root`]; a leading
/// `/` is WPT's root).
fn resolve(base: &str, href: &str) -> String {
    let href = href.split(['#', '?']).next().unwrap_or(href);
    let mut parts: Vec<&str> = if href.starts_with('/') {
        Vec::new()
    } else {
        base.split('/').collect()
    };
    if !href.starts_with('/') {
        parts.pop();
    }
    for segment in href.trim_start_matches('/').split('/') {
        match segment {
            "." | "" => {}
            ".." => {
                parts.pop();
            }
            segment => parts.push(segment),
        }
    }
    parts.join("/")
}

fn reftests() -> Vec<Reftest> {
    let mut files = Vec::new();
    let mut directories = vec![root()];
    while let Some(directory) = directories.pop() {
        for entry in std::fs::read_dir(&directory).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                directories.push(path);
            } else if path.extension().is_some_and(|extension| {
                ["html", "htm", "xht", "xhtml"].contains(&&*extension.to_string_lossy())
            }) {
                files.push(path);
            }
        }
    }
    let mut tests = Vec::new();
    for file in files {
        let path = file
            .strip_prefix(root())
            .unwrap()
            .to_string_lossy()
            .replace('\\', "/");
        let html = std::fs::read_to_string(&file).unwrap();
        let references: Vec<(Relation, String)> = tags(&html, "link")
            .into_iter()
            .filter_map(|tag| {
                let relation = match attribute(tag, "rel")?.to_ascii_lowercase().as_str() {
                    "match" => Relation::Match,
                    "mismatch" => Relation::Mismatch,
                    _ => return None,
                };
                Some((relation, resolve(&path, attribute(tag, "href")?)))
            })
            .collect();
        // References are files too; tests are the files with references of their own
        // that are all here (a reference can be another test, whose references the
        // selection didn't take).
        if references.is_empty()
            || path.contains("-ref")
            || path.contains("/reference/")
            || references
                .iter()
                .any(|(_, reference)| !root().join(reference).exists())
        {
            continue;
        }
        let fuzzy = tags(&html, "meta")
            .into_iter()
            .find(|tag| {
                attribute(tag, "name").is_some_and(|name| name.eq_ignore_ascii_case("fuzzy"))
            })
            .and_then(|tag| attribute(tag, "content"))
            .map_or(Fuzzy::EXACT, Fuzzy::parse);
        tests.push(Reftest {
            path,
            references,
            fuzzy,
        });
    }
    tests.sort_by(|a, b| a.path.cmp(&b.path));
    tests
}

/// Points the relative image URLs of a test file (`src`, `url(…)`) at
/// [`IMAGE_ORIGIN`], so they load the way a message's remote images do.
fn absolute_images(html: &str, path: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut rest = html;
    while let Some(at) = rest.find(['.']) {
        let (before, after) = rest.split_at(at);
        let extension = ["png", "gif", "jpg", "jpeg"]
            .into_iter()
            .find(|extension| after[1..].to_ascii_lowercase().starts_with(extension));
        let Some(extension) = extension else {
            out.push_str(&rest[..=at]);
            rest = &rest[at + 1..];
            continue;
        };
        // The URL runs back to a quote, `(` or `=`.
        let start = before
            .rfind(['"', '\'', '(', '=', ' '])
            .map_or(0, |start| start + 1);
        let url = &rest[start..at + 1 + extension.len()];
        out.push_str(&rest[..start]);
        if url.contains(':') {
            out.push_str(url);
        } else {
            out.push_str(IMAGE_ORIGIN);
            out.push_str(&resolve(path, url));
        }
        rest = &rest[at + 1 + extension.len()..];
    }
    out.push_str(rest);
    out
}

/// Serves the test images from [`root`].
struct Images;

impl PartSource for Images {
    fn inline_part(&self, _content_id: &str) -> Option<Fetch> {
        None
    }

    fn remote(&self, url: &str) -> Option<Fetch> {
        let path = root().join(url.strip_prefix(IMAGE_ORIGIN)?);
        let content_type = match path.extension()?.to_string_lossy().as_ref() {
            "png" => "image/png",
            "gif" => "image/gif",
            _ => "image/jpeg",
        };
        Some(Box::new(move || {
            Some(Part {
                content_type: content_type.into(),
                bytes: std::fs::read(path).ok()?,
            })
        }))
    }
}

/// Renders the file at `path` (relative to [`root`]) the way the original view
/// renders a message.
fn render(path: &str) -> image::RgbaImage {
    let html = std::fs::read_to_string(root().join(path)).unwrap();
    let html = absolute_images(&html, path);
    let sanitized = mazemail_mime::html::sanitize(&html, &Default::default());
    let options = DocumentOptions {
        remote_allowed: true,
        colors: None,
    };
    let document = OriginalDocument::build(&sanitized.html, &options, 1);
    let inbox = Rc::new(Inbox::default());
    let page = Page::new(
        document,
        Rc::new(Images),
        inbox.clone(),
        Arc::new(|| {}),
        (800, 600, 1.),
    );
    page.load();
    let deadline = Instant::now() + Duration::from_secs(10);
    while inbox.loaded.get() != Some(1) {
        assert!(Instant::now() < deadline, "{path} never finished loading");
        page.poll();
        std::thread::sleep(Duration::from_millis(1));
    }
    // Images arrive and decode; stop once nothing changed for a while.
    let mut image = page.paint().unwrap();
    let mut quiet = 0;
    while quiet < 15 && Instant::now() < deadline {
        page.poll();
        if inbox.frame_ready.replace(false) {
            image = page.paint().unwrap();
            quiet = 0;
        } else {
            quiet += 1;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    image
}

/// Whether `a` and `b` look the same within `fuzzy`.
fn same(a: &image::RgbaImage, b: &image::RgbaImage, fuzzy: Fuzzy) -> bool {
    let mut differing = 0;
    for (x, y) in a.pixels().zip(b.pixels()) {
        let difference = x.0[..3]
            .iter()
            .zip(&y.0[..3])
            .map(|(&p, &q)| p.abs_diff(q))
            .max()
            .unwrap_or(0);
        if difference > 0 {
            if difference > fuzzy.max_difference {
                return false;
            }
            differing += 1;
        }
    }
    differing <= fuzzy.total_pixels
}

/// Whether the test passes. With `MAZEMAIL_WPT_OUT=<dir>`, a failing test leaves its
/// rendering and its references' there, to look at.
fn passes(test: &Reftest) -> bool {
    let rendered = render(&test.path);
    let mut pass = true;
    let mut references = Vec::new();
    for (relation, reference) in &test.references {
        let image = render(reference);
        pass &= same(&rendered, &image, test.fuzzy) == (*relation == Relation::Match);
        references.push(image);
    }
    if let (false, Some(out)) = (pass, std::env::var_os("MAZEMAIL_WPT_OUT")) {
        let name = test.path.replace('/', "__");
        let out = PathBuf::from(out);
        rendered.save(out.join(format!("{name}.test.png"))).unwrap();
        for (n, image) in references.iter().enumerate() {
            image.save(out.join(format!("{name}.ref{n}.png"))).unwrap();
        }
    }
    pass
}

#[test]
fn wpt_reftests() {
    let tests = reftests();
    assert!(!tests.is_empty(), "no reftests in {}", root().display());
    let expectations_path = root().join("expectations.txt");
    // `FAIL <path>`, optionally followed by `# <what it's waiting for>`.
    let notes: std::collections::HashMap<String, String> =
        std::fs::read_to_string(&expectations_path)
            .unwrap_or_default()
            .lines()
            .filter_map(|line| line.strip_prefix("FAIL "))
            .map(|rest| match rest.split_once('#') {
                Some((path, note)) => (path.trim().to_owned(), note.trim().to_owned()),
                None => (rest.trim().to_owned(), String::new()),
            })
            .collect();
    let expected_failures: Vec<String> = notes.keys().cloned().collect();

    // Every thread renders with pages of its own.
    let next = AtomicUsize::new(0);
    let threads = std::thread::available_parallelism().map_or(4, |n| n.get().min(8));
    let mut results: Vec<(String, bool)> = std::thread::scope(|scope| {
        let workers: Vec<_> = (0..threads)
            .map(|_| {
                scope.spawn(|| {
                    let mut results = Vec::new();
                    while let Some(test) = tests.get(next.fetch_add(1, Ordering::Relaxed)) {
                        results.push((test.path.clone(), passes(test)));
                    }
                    results
                })
            })
            .collect();
        workers
            .into_iter()
            .flat_map(|worker| worker.join().unwrap())
            .collect()
    });
    results.sort();

    let passed = results.iter().filter(|(_, pass)| *pass).count();
    eprintln!("WPT reftests: {passed} of {} pass", results.len());
    if std::env::var_os("MAZEMAIL_WPT_UPDATE").is_some() {
        let mut list = String::from(
            "# WPT reftests that fail in the original view today (see README.maze.md), each\n\
             # with what it's waiting for. Regenerate with MAZEMAIL_WPT_UPDATE=1 (notes stay).\n",
        );
        for (path, _) in results.iter().filter(|(_, pass)| !pass) {
            match notes.get(path).filter(|note| !note.is_empty()) {
                Some(note) => list.push_str(&format!("FAIL {path}  # {note}\n")),
                None => list.push_str(&format!("FAIL {path}\n")),
            }
        }
        std::fs::write(&expectations_path, list).unwrap();
        return;
    }
    let newly_failing: Vec<&str> = results
        .iter()
        .filter(|(path, pass)| !pass && !expected_failures.contains(path))
        .map(|(path, _)| path.as_str())
        .collect();
    let newly_passing: Vec<&str> = results
        .iter()
        .filter(|(path, pass)| *pass && expected_failures.contains(path))
        .map(|(path, _)| path.as_str())
        .collect();
    assert!(
        newly_failing.is_empty() && newly_passing.is_empty(),
        "WPT reftests changed (update tests/wpt/expectations.txt, MAZEMAIL_WPT_UPDATE=1):\n\
         failing now: {newly_failing:#?}\npassing now: {newly_passing:#?}"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fuzzy_forms() {
        let f = Fuzzy::parse("maxDifference=0-2;totalPixels=0-40");
        assert_eq!((f.max_difference, f.total_pixels), (2, 40));
        let f = Fuzzy::parse("ref.html:5;100");
        assert_eq!((f.max_difference, f.total_pixels), (5, 100));
    }

    #[test]
    fn links_and_paths() {
        let tag = r#"<link rel="match" href="../reference/a-ref.html">"#;
        assert_eq!(attribute(tag, "rel"), Some("match"));
        assert_eq!(
            resolve("css/css-tables/x.html", "../reference/a-ref.html"),
            "css/reference/a-ref.html"
        );
        assert_eq!(
            resolve("css/css-tables/x.html", "/css/reference/b.xht"),
            "css/reference/b.xht"
        );
        assert_eq!(
            absolute_images(r#"<img src="support/a.png"> url(b.gif)"#, "css/t/x.html"),
            r#"<img src="https://wpt.example/css/t/support/a.png"> url(https://wpt.example/css/t/b.gif)"#
        );
    }
}
