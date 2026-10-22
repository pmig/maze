//! Maildir reader and writer (`cur/`, `new/`, `tmp/`, `:2,` info flags).
//!
//! Flags in the info suffix: `P` passed (forwarded), `R` replied, `S` seen, `T`
//! trashed (`\Deleted`), `D` draft, `F` flagged. Lowercase letters are Dovecot keyword
//! slots, named in the folder's `dovecot-keywords` file; the reader maps them and the
//! writer maintains that file. Thunderbird's own Maildir store keeps flags in
//! `X-Mozilla-*` headers instead, which [`reconstruct`](crate::reconstruct) reads.

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use mazemail_types::Flags;

use crate::{Error, Result};

/// Name of Dovecot's keyword table in a Maildir folder.
pub const DOVECOT_KEYWORDS: &str = "dovecot-keywords";

/// Separators between unique name and info: `:` normally, `!` or `;` where the file
/// system doesn't allow colons.
const INFO_SEPARATORS: [char; 3] = [':', '!', ';'];

/// One message file in a Maildir.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MaildirEntry {
    pub path: PathBuf,
    /// In `new/` (not seen by any client yet).
    pub is_new: bool,
    /// Flags from the info suffix.
    pub flags: Flags,
    /// Keywords from Dovecot keyword letters.
    pub keywords: Vec<String>,
    /// The delivery time from the file's modification time, Unix seconds.
    pub mtime: Option<i64>,
}

/// Parses the `:2,<flags>` suffix of a file name: flags and keyword letters.
pub fn parse_info(name: &str) -> (Flags, Vec<char>) {
    let mut flags = Flags::empty();
    let mut letters = Vec::new();
    let info = INFO_SEPARATORS
        .iter()
        .find_map(|sep| name.rsplit_once(*sep).map(|(_, info)| info))
        .and_then(|info| info.strip_prefix("2,"));
    if let Some(info) = info {
        for c in info.chars() {
            match c {
                'P' => flags |= Flags::FORWARDED,
                'R' => flags |= Flags::ANSWERED,
                'S' => flags |= Flags::SEEN,
                'T' => flags |= Flags::DELETED,
                'D' => flags |= Flags::DRAFT,
                'F' => flags |= Flags::FLAGGED,
                'a'..='z' => letters.push(c),
                _ => {}
            }
        }
    }
    (flags, letters)
}

/// The info suffix (`:2,FRS`) for flags and keyword letters, letters sorted as the
/// spec requires.
pub fn format_info(flags: Flags, letters: &[char]) -> String {
    let mut chars: Vec<char> = [
        (Flags::DRAFT, 'D'),
        (Flags::FLAGGED, 'F'),
        (Flags::FORWARDED, 'P'),
        (Flags::ANSWERED, 'R'),
        (Flags::SEEN, 'S'),
        (Flags::DELETED, 'T'),
    ]
    .into_iter()
    .filter(|(flag, _)| flags.contains(*flag))
    .map(|(_, c)| c)
    .collect();
    let mut letters = letters.to_vec();
    letters.sort_unstable();
    letters.dedup();
    chars.extend(letters);
    format!(":2,{}", chars.into_iter().collect::<String>())
}

/// Reads `dovecot-keywords` (`<index> <keyword>` per line).
fn read_keywords(dir: &Path) -> BTreeMap<char, String> {
    let Ok(text) = fs::read_to_string(dir.join(DOVECOT_KEYWORDS)) else {
        return BTreeMap::new();
    };
    text.lines()
        .filter_map(|line| {
            let (index, keyword) = line.trim().split_once(char::is_whitespace)?;
            let index: u8 = index.parse().ok()?;
            (index < 26).then(|| (char::from(b'a' + index), keyword.trim().to_owned()))
        })
        .collect()
}

/// Whether `dir` is a Maildir folder (has a `cur` directory).
pub fn is_maildir(dir: &Path) -> bool {
    dir.join("cur").is_dir()
}

/// The messages of one Maildir folder (`cur/` and `new/`), ordered by file name
/// (which starts with the delivery time). Dot files and `tmp/` are ignored.
pub fn list(dir: &Path) -> Result<Vec<MaildirEntry>> {
    let keywords = read_keywords(dir);
    let mut out = Vec::new();
    for (sub, is_new) in [("cur", false), ("new", true)] {
        let path = dir.join(sub);
        let entries = match fs::read_dir(&path) {
            Ok(entries) => entries,
            Err(err) if err.kind() == io::ErrorKind::NotFound => continue,
            Err(err) => return Err(Error::io("listing", path)(err)),
        };
        for entry in entries {
            let entry = entry.map_err(Error::io("listing", &path))?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') {
                continue;
            }
            let meta = entry
                .metadata()
                .map_err(Error::io("reading", entry.path()))?;
            if !meta.is_file() {
                continue;
            }
            let (flags, letters) = if is_new {
                (Flags::empty(), Vec::new())
            } else {
                parse_info(&name)
            };
            let mut kw: Vec<String> = letters
                .iter()
                .filter_map(|c| keywords.get(c).map(|k| k.to_lowercase()))
                .collect();
            kw.sort();
            kw.dedup();
            out.push(MaildirEntry {
                path: entry.path(),
                is_new,
                flags,
                keywords: kw,
                mtime: meta
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                    .map(|d| d.as_secs() as i64),
            });
        }
    }
    out.sort_by(|a, b| a.path.file_name().cmp(&b.path.file_name()));
    Ok(out)
}

/// Maildir++ subfolders of a Maildir root (`.Sent`, `.Archive.2024`), as
/// `(folder path segments, directory)`, sorted.
pub fn maildirpp_subfolders(root: &Path) -> Result<Vec<(Vec<String>, PathBuf)>> {
    let mut out = Vec::new();
    let entries = fs::read_dir(root).map_err(Error::io("listing", root))?;
    for entry in entries {
        let entry = entry.map_err(Error::io("listing", root))?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(path) = name.strip_prefix('.') else {
            continue;
        };
        if path.is_empty() || path == "." || !is_maildir(&entry.path()) {
            continue;
        }
        let segments: Vec<String> = path
            .split('.')
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .collect();
        if !segments.is_empty() {
            out.push((segments, entry.path()));
        }
    }
    out.sort();
    Ok(out)
}

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// Writes messages into a Maildir folder: `tmp/` first, then renamed into `cur/` with
/// their flags. Keywords get Dovecot letters, saved by [`MaildirWriter::finish`].
#[derive(Debug)]
pub struct MaildirWriter {
    dir: PathBuf,
    keywords: BTreeMap<String, char>,
    written: u64,
}

impl MaildirWriter {
    /// Creates `dir` with `cur/`, `new/` and `tmp/` (mode 0700 on Unix).
    pub fn create(dir: &Path) -> Result<Self> {
        for sub in ["cur", "new", "tmp"] {
            let path = dir.join(sub);
            create_dir_private(&path)?;
        }
        let keywords = read_keywords(dir)
            .into_iter()
            .map(|(c, k)| (k.to_lowercase(), c))
            .collect();
        Ok(Self {
            dir: dir.to_owned(),
            keywords,
            written: 0,
        })
    }

    /// Messages written.
    pub fn count(&self) -> u64 {
        self.written
    }

    /// Writes one message with flags and keywords; returns its path. Keywords beyond
    /// the 26 Dovecot slots are dropped (with a debug log).
    pub fn write(&mut self, message: &[u8], flags: Flags, keywords: &[String]) -> Result<PathBuf> {
        let mut letters = Vec::new();
        for keyword in keywords {
            let keyword = keyword.to_lowercase();
            let next = self.keywords.len();
            let letter = match self.keywords.get(&keyword) {
                Some(c) => Some(*c),
                None if next < 26 => {
                    let c = char::from(b'a' + next as u8);
                    self.keywords.insert(keyword, c);
                    Some(c)
                }
                None => {
                    tracing::debug!("more than 26 keywords in a Maildir folder; one dropped");
                    None
                }
            };
            letters.extend(letter);
        }
        let unique = unique_name();
        let tmp = self.dir.join("tmp").join(&unique);
        {
            let mut file = create_file_private(&tmp)?;
            file.write_all(message)
                .map_err(Error::io("writing", &tmp))?;
            file.sync_all().map_err(Error::io("syncing", &tmp))?;
        }
        let target = self
            .dir
            .join("cur")
            .join(format!("{unique}{}", format_info(flags, &letters)));
        fs::rename(&tmp, &target).map_err(Error::io("renaming", &tmp))?;
        self.written += 1;
        Ok(target)
    }

    /// Writes `dovecot-keywords` if any keyword was used.
    pub fn finish(self) -> Result<()> {
        if self.keywords.is_empty() {
            return Ok(());
        }
        let mut by_letter: Vec<(char, &String)> =
            self.keywords.iter().map(|(k, c)| (*c, k)).collect();
        by_letter.sort();
        let text: String = by_letter
            .into_iter()
            .map(|(c, k)| format!("{} {k}\n", u32::from(c) - u32::from('a')))
            .collect();
        let path = self.dir.join(DOVECOT_KEYWORDS);
        fs::write(&path, text).map_err(Error::io("writing", path))
    }
}

/// `<secs>.M<micros>P<pid>Q<counter>.mazemail`: unique per process and call.
fn unique_name() -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    format!(
        "{}.M{}P{}Q{}.mazemail",
        now.as_secs(),
        now.subsec_micros(),
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    )
}

/// Creates a directory (and parents) readable only by the user.
pub(crate) fn create_dir_private(path: &Path) -> Result<()> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(path).map_err(Error::io("creating", path))
}

/// Creates a new file readable only by the user.
pub(crate) fn create_file_private(path: &Path) -> Result<File> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path).map_err(Error::io("creating", path))
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    #[test]
    fn info_round_trips() {
        let (flags, letters) = parse_info("1700000000.M1P2.host:2,FRSab");
        assert_eq!(flags, Flags::FLAGGED | Flags::ANSWERED | Flags::SEEN);
        assert_eq!(letters, vec!['a', 'b']);
        assert_eq!(format_info(flags, &['b', 'a']), ":2,FRSab");
        assert_eq!(parse_info("x!2,T").0, Flags::DELETED);
        assert_eq!(parse_info("no-info").0, Flags::empty());
        assert_eq!(
            format_info(Flags::all(), &[]),
            ":2,DFPRST",
            "junk flags have no Maildir letter"
        );
    }

    #[test]
    fn writes_and_lists_with_keywords() {
        let dir = tempfile::tempdir().unwrap();
        let folder = dir.path().join("Inbox");
        let mut writer = MaildirWriter::create(&folder).unwrap();
        writer
            .write(
                b"Subject: a\r\n\r\nA\r\n",
                Flags::SEEN,
                &["$label1".to_owned()],
            )
            .unwrap();
        writer
            .write(
                b"Subject: b\r\n\r\nB\r\n",
                Flags::FLAGGED,
                &["Todo".to_owned(), "$label1".to_owned()],
            )
            .unwrap();
        writer.finish().unwrap();
        fs::write(
            folder.join("new").join("1.new.host"),
            b"Subject: c\r\n\r\nC\r\n",
        )
        .unwrap();
        fs::write(folder.join("cur").join(".hidden"), b"x").unwrap();

        assert!(is_maildir(&folder));
        let entries = list(&folder).unwrap();
        assert_eq!(entries.len(), 3);
        let by_flags: Vec<(Flags, Vec<String>, bool)> = entries
            .iter()
            .map(|e| (e.flags, e.keywords.clone(), e.is_new))
            .collect();
        assert!(by_flags.contains(&(Flags::SEEN, vec!["$label1".to_owned()], false)));
        assert!(by_flags.contains(&(
            Flags::FLAGGED,
            vec!["$label1".to_owned(), "todo".to_owned()],
            false
        )));
        assert!(by_flags.contains(&(Flags::empty(), vec![], true)));
        assert_eq!(
            fs::read_to_string(folder.join(DOVECOT_KEYWORDS)).unwrap(),
            "0 $label1\n1 todo\n"
        );
    }

    #[test]
    fn finds_maildirpp_subfolders() {
        let dir = tempfile::tempdir().unwrap();
        MaildirWriter::create(dir.path()).unwrap();
        MaildirWriter::create(&dir.path().join(".Archive.2024")).unwrap();
        MaildirWriter::create(&dir.path().join(".Sent")).unwrap();
        fs::create_dir(dir.path().join(".not-a-maildir")).unwrap();
        let subs = maildirpp_subfolders(dir.path()).unwrap();
        let names: Vec<Vec<String>> = subs.into_iter().map(|(s, _)| s).collect();
        assert_eq!(
            names,
            vec![
                vec!["Archive".to_owned(), "2024".to_owned()],
                vec!["Sent".to_owned()]
            ]
        );
    }
}
