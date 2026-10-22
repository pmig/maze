//! Streaming mbox reader and writer (mboxrd and mboxo).
//!
//! # Reading
//!
//! [`MboxReader`] reads any [`BufRead`] line by line in constant memory (one message
//! at a time) and yields [`MboxRecord`]s with their byte range in the file. It follows
//! Thunderbird's parser (`mailnews/base/src/MboxMsgInputStream.cpp`):
//!
//! - A record starts with a `From ` envelope line. Inside a message, a `From ` line is
//!   a separator only if the next two lines look like header fields (or the input ends
//!   first), so unescaped `From ` lines in badly written bodies don't split messages.
//! - The blank line before a separator (and at the end of the file) belongs to the
//!   mbox format, not the message, and is removed.
//! - `>From ` unescaping per [`Escaping`]: mboxrd removes one `>` from `>+From `,
//!   mboxo only from `>From `.
//! - Bytes before the first envelope line are skipped and counted
//!   ([`MboxReader::skipped_bytes`]). A truncated last message is returned as far as
//!   it goes.
//!
//! Line endings are kept as they are; [`reconstruct`](crate::reconstruct) normalizes
//! them.
//!
//! # Writing
//!
//! [`MboxWriter`] is the reverse: envelope line, optional Thunderbird status headers,
//! the message with mboxrd escaping, a final line ending if missing, and a blank line.
//!
//! # Memory mapping
//!
//! The reader works on buffered reads (1 MiB buffers) instead of `mmap`. The ignored
//! `perf` integration test scans a generated 8 GB tree (254k messages) for the body
//! reuse index at about 8 GB/s with a warm page cache on an Apple Silicon Mac (all
//! cores, reconstruction, header parsing and hashing included), so reading is never
//! the bottleneck; buffered reads also keep memory bounded and need no `unsafe` (a
//! mapped file that Thunderbird truncates while we read would be undefined behavior).

use std::collections::VecDeque;
use std::io::{self, BufRead, Write};

use time::{Date, Month, OffsetDateTime, PrimitiveDateTime, Time};

use crate::mozilla::{MozillaState, memchr_newline, trim_eol};

/// How `From ` lines inside messages were escaped.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum Escaping {
    /// mboxrd (Thunderbird, Dovecot): any `>*From ` line got one more `>`.
    #[default]
    Mboxrd,
    /// mboxo: only `From ` lines got a `>` (ambiguous, but common).
    Mboxo,
    /// No escaping (mboxcl/mboxcl2 rely on Content-Length; rarely seen).
    None,
}

/// The `From ` envelope line of a record.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Envelope {
    /// The envelope sender (`-` for Thunderbird, `MAILER-DAEMON`, an address, …).
    pub sender: Option<String>,
    /// The delivery time, Unix seconds (UTC); `None` if the line has no readable date.
    pub date: Option<i64>,
}

impl Envelope {
    /// Parses the text after `From ` (`- Tue Dec 09 15:30:45 2014`).
    pub fn parse(line: &[u8]) -> Self {
        let text = String::from_utf8_lossy(trim_eol(line));
        let rest = text.strip_prefix("From ").unwrap_or(&text);
        let mut tokens = rest.split_whitespace();
        let sender = tokens.next().map(str::to_owned);
        let date = parse_asctime(&tokens.collect::<Vec<_>>());
        Self { sender, date }
    }

    /// The envelope line (`From <sender> <asctime>`), without line ending.
    pub fn to_line(&self) -> String {
        let sender = self.sender.as_deref().unwrap_or("-");
        let sender = if sender.is_empty() || sender.contains(char::is_whitespace) {
            "-"
        } else {
            sender
        };
        format!("From {sender} {}", format_asctime(self.date.unwrap_or(0)))
    }
}

const MONTHS: [&str; 12] = [
    "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
];

/// Parses ctime-like dates (`Tue Dec 09 15:30:45 2014`, `Tue Dec  9 15:30:45 +0100 2014`,
/// `Tue, 09 Dec 2014 15:30:45`): finds the month name, then day, time and year around
/// it. A numeric zone token is applied when present.
fn parse_asctime(tokens: &[&str]) -> Option<i64> {
    let month_index = tokens.iter().position(|t| {
        let t = t.trim_matches(',').to_ascii_lowercase();
        t.len() >= 3 && MONTHS.contains(&&t[..3])
    })?;
    let month_name = tokens[month_index].to_ascii_lowercase();
    let month_number = MONTHS.iter().position(|m| month_name.starts_with(m))? as u8 + 1;
    let month = Month::try_from(month_number).ok()?;
    let mut day = None;
    let mut clock = None;
    let mut year = None;
    let mut offset_secs = 0i64;
    for (i, token) in tokens.iter().enumerate() {
        if i == month_index {
            continue;
        }
        let token = token.trim_matches(',');
        if token.contains(':') {
            let mut parts = token.split(':').map(|p| p.parse::<u8>().ok());
            let h = parts.next().flatten()?;
            let m = parts.next().flatten()?;
            let s = parts.next().flatten().unwrap_or(0);
            clock = Time::from_hms(h, m, s).ok();
        } else if (token.starts_with('+') || token.starts_with('-')) && token.len() == 5 {
            if let Ok(value) = token[1..].parse::<i64>() {
                let secs = (value / 100) * 3600 + (value % 100) * 60;
                offset_secs = if token.starts_with('-') { -secs } else { secs };
            }
        } else if let Ok(number) = token.parse::<i32>() {
            if token.len() == 4 {
                year = Some(number);
            } else if day.is_none() && (1..=31).contains(&number) {
                day = u8::try_from(number).ok();
            }
        }
    }
    let date = Date::from_calendar_date(year?, month, day?).ok()?;
    let at = PrimitiveDateTime::new(date, clock.unwrap_or(Time::MIDNIGHT));
    Some(at.assume_utc().unix_timestamp() - offset_secs)
}

/// `Tue Dec 09 15:30:45 2014` in UTC, as Thunderbird writes it.
fn format_asctime(unix: i64) -> String {
    let at = OffsetDateTime::from_unix_timestamp(unix).unwrap_or(OffsetDateTime::UNIX_EPOCH);
    let weekday = &at.weekday().to_string()[..3];
    let month = &at.month().to_string()[..3];
    format!(
        "{weekday} {month} {:02} {:02}:{:02}:{:02} {}",
        at.day(),
        at.hour(),
        at.minute(),
        at.second(),
        at.year()
    )
}

/// One message read from an mbox file.
#[derive(Clone, PartialEq, Eq)]
pub struct MboxRecord {
    /// Byte offset of the envelope line in the file.
    pub offset: u64,
    /// Length of the record in the file, from the envelope line to the next one (or
    /// the end of the file).
    pub len: u64,
    pub envelope: Envelope,
    /// The message: unescaped, line endings as stored, the separating blank line
    /// removed.
    pub message: Vec<u8>,
}

impl std::fmt::Debug for MboxRecord {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MboxRecord")
            .field("offset", &self.offset)
            .field("len", &self.len)
            .field("envelope_date", &self.envelope.date)
            .field("bytes", &self.message.len())
            .finish()
    }
}

/// Line source with push-back and buffer reuse.
struct Lines<R> {
    inner: R,
    pending: VecDeque<Vec<u8>>,
    spare: Vec<Vec<u8>>,
    /// Bytes handed out and not pushed back.
    consumed: u64,
}

impl<R: BufRead> Lines<R> {
    fn next(&mut self) -> io::Result<Option<Vec<u8>>> {
        let line = match self.pending.pop_front() {
            Some(line) => line,
            None => {
                let mut buf = self.spare.pop().unwrap_or_default();
                buf.clear();
                if self.inner.read_until(b'\n', &mut buf)? == 0 {
                    self.spare.push(buf);
                    return Ok(None);
                }
                buf
            }
        };
        self.consumed += line.len() as u64;
        Ok(Some(line))
    }

    fn unread(&mut self, line: Vec<u8>) {
        self.consumed -= line.len() as u64;
        self.pending.push_front(line);
    }

    fn recycle(&mut self, line: Vec<u8>) {
        if self.spare.len() < 8 {
            self.spare.push(line);
        }
    }
}

fn is_from_line(line: &[u8]) -> bool {
    line.starts_with(b"From ")
}

/// A header field line: printable ASCII name without colon, then a colon.
fn looks_like_header(line: &[u8]) -> bool {
    match line
        .iter()
        .position(|&b| !(33..=126).contains(&b) || b == b':')
    {
        Some(i) => i > 0 && line[i] == b':',
        None => false,
    }
}

/// Reads mbox records from a buffered reader (see the module docs).
pub struct MboxReader<R> {
    lines: Lines<R>,
    escaping: Escaping,
    skipped: u64,
    /// The envelope line of the next record, already read.
    next_from: Option<(u64, Vec<u8>)>,
    started: bool,
}

impl<R> std::fmt::Debug for MboxReader<R> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MboxReader")
            .field("escaping", &self.escaping)
            .field("skipped", &self.skipped)
            .finish_non_exhaustive()
    }
}

impl<R: BufRead> MboxReader<R> {
    /// A reader with mboxrd unescaping.
    pub fn new(inner: R) -> Self {
        Self {
            lines: Lines {
                inner,
                pending: VecDeque::new(),
                spare: Vec::new(),
                consumed: 0,
            },
            escaping: Escaping::Mboxrd,
            skipped: 0,
            next_from: None,
            started: false,
        }
    }

    /// Sets the escaping variant.
    pub fn with_escaping(mut self, escaping: Escaping) -> Self {
        self.escaping = escaping;
        self
    }

    /// Bytes before the first envelope line, which were skipped.
    pub fn skipped_bytes(&self) -> u64 {
        self.skipped
    }

    /// Whether the `From ` line just read separates messages: the next two lines look
    /// like headers, or the input ends before they show otherwise.
    fn is_separator(&mut self) -> io::Result<bool> {
        let mut peeked = Vec::with_capacity(2);
        let mut verdict = true;
        for _ in 0..2 {
            match self.lines.next()? {
                Some(line) => {
                    let ok = looks_like_header(&line);
                    peeked.push(line);
                    if !ok {
                        verdict = false;
                        break;
                    }
                }
                None => break,
            }
        }
        for line in peeked.into_iter().rev() {
            self.lines.unread(line);
        }
        Ok(verdict)
    }

    /// Finds the first envelope line, skipping anything before it.
    fn find_first(&mut self) -> io::Result<Option<(u64, Vec<u8>)>> {
        loop {
            let offset = self.lines.consumed;
            let Some(line) = self.lines.next()? else {
                return Ok(None);
            };
            // At the very start a `From ` line is trusted as is; after garbage it must
            // look like a real separator.
            if is_from_line(&line) && (offset == 0 || self.is_separator()?) {
                return Ok(Some((offset, line)));
            }
            self.skipped += line.len() as u64;
            self.lines.recycle(line);
        }
    }

    fn read_record(&mut self) -> io::Result<Option<MboxRecord>> {
        if !self.started {
            self.started = true;
            self.next_from = self.find_first()?;
        }
        let Some((offset, from_line)) = self.next_from.take() else {
            return Ok(None);
        };
        let envelope = Envelope::parse(&from_line);
        self.lines.recycle(from_line);
        let mut message = Vec::new();
        // Length of the last line appended if it was blank (the separator candidate).
        let mut trailing_blank: Option<usize> = None;
        loop {
            let line_offset = self.lines.consumed;
            let Some(line) = self.lines.next()? else {
                break;
            };
            if is_from_line(&line) && self.is_separator()? {
                self.next_from = Some((line_offset, line));
                break;
            }
            let start = match self.escaping {
                Escaping::Mboxrd => {
                    let quotes = line.iter().take_while(|&&b| b == b'>').count();
                    usize::from(quotes > 0 && line[quotes..].starts_with(b"From "))
                }
                Escaping::Mboxo => usize::from(line.starts_with(b">From ")),
                Escaping::None => 0,
            };
            let content = &line[start..];
            trailing_blank = trim_eol(content).is_empty().then_some(content.len());
            message.extend_from_slice(content);
            self.lines.recycle(line);
        }
        if let Some(blank) = trailing_blank.filter(|&n| n > 0) {
            message.truncate(message.len() - blank);
        }
        let end = self
            .next_from
            .as_ref()
            .map_or(self.lines.consumed, |(next, _)| *next);
        Ok(Some(MboxRecord {
            offset,
            len: end - offset,
            envelope,
            message,
        }))
    }
}

impl<R: BufRead> Iterator for MboxReader<R> {
    type Item = io::Result<MboxRecord>;

    fn next(&mut self) -> Option<Self::Item> {
        self.read_record().transpose()
    }
}

/// Reads the single record at the start of `bytes` (a range cut out of an mbox file
/// by its offset and length).
pub fn read_single_record(bytes: &[u8]) -> Option<MboxRecord> {
    MboxReader::new(bytes).next().and_then(Result::ok)
}

/// Writes messages as mboxrd (see the module docs).
#[derive(Debug)]
pub struct MboxWriter<W> {
    inner: W,
    written: u64,
}

impl<W: Write> MboxWriter<W> {
    /// A writer appending to `inner`.
    pub fn new(inner: W) -> Self {
        Self { inner, written: 0 }
    }

    /// Bytes written so far.
    pub fn bytes_written(&self) -> u64 {
        self.written
    }

    /// Writes one message: envelope line, optional Thunderbird status headers, the
    /// message with mboxrd escaping, a final line ending if missing, and the
    /// separating blank line. Line endings follow the message's first line.
    pub fn write_message(
        &mut self,
        envelope: &Envelope,
        mozilla: Option<&MozillaState>,
        message: &[u8],
    ) -> io::Result<()> {
        let eol: &[u8] = match memchr_newline(message) {
            Some(i) if i > 0 && message[i - 1] == b'\r' => b"\r\n",
            Some(_) => b"\n",
            None => b"\r\n",
        };
        let mut out = Vec::with_capacity(message.len() + 256);
        out.extend_from_slice(envelope.to_line().as_bytes());
        out.extend_from_slice(eol);
        if let Some(state) = mozilla {
            out.extend_from_slice(&state.header_lines(eol));
        }
        let mut pos = 0;
        while pos < message.len() {
            let end = memchr_newline(&message[pos..]).map_or(message.len(), |i| pos + i + 1);
            let line = &message[pos..end];
            let quotes = line.iter().take_while(|&&b| b == b'>').count();
            if line[quotes..].starts_with(b"From ") {
                out.push(b'>');
            }
            out.extend_from_slice(line);
            pos = end;
        }
        if !message.is_empty() && !message.ends_with(b"\n") {
            out.extend_from_slice(eol);
        }
        out.extend_from_slice(eol);
        self.inner.write_all(&out)?;
        self.written += out.len() as u64;
        Ok(())
    }

    /// Flushes and returns the inner writer.
    pub fn finish(mut self) -> io::Result<W> {
        self.inner.flush()?;
        Ok(self.inner)
    }
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    fn read_all(input: &[u8], escaping: Escaping) -> Vec<MboxRecord> {
        MboxReader::new(input)
            .with_escaping(escaping)
            .collect::<io::Result<Vec<_>>>()
            .unwrap()
    }

    #[test]
    fn parses_envelope_lines() {
        let e = Envelope::parse(b"From - Tue Dec 09 15:30:45 2014\r\n");
        assert_eq!(e.sender.as_deref(), Some("-"));
        assert_eq!(e.date, Some(1_418_139_045));
        let e = Envelope::parse(b"From anna@example.com Tue Dec  9 16:30:45 +0100 2014\n");
        assert_eq!(e.sender.as_deref(), Some("anna@example.com"));
        assert_eq!(e.date, Some(1_418_139_045));
        let e = Envelope::parse(b"From MAILER-DAEMON\n");
        assert_eq!(e.date, None);
        let e = Envelope {
            sender: None,
            date: Some(1_418_139_045),
        };
        assert_eq!(e.to_line(), "From - Tue Dec 09 15:30:45 2014");
        let line = e.to_line();
        let mut expected = e;
        expected.sender = Some("-".to_owned());
        assert_eq!(Envelope::parse(line.as_bytes()), expected);
    }

    #[test]
    fn splits_records_and_tracks_offsets() {
        let input = b"From - Mon Jan 01 00:00:00 2024\n\
Subject: one\n\
From: a@x\n\
\n\
Body one\n\
\n\
From - Mon Jan 01 00:00:01 2024\n\
Subject: two\n\
From: b@x\n\
\n\
Body two\n";
        let records = read_all(input, Escaping::Mboxrd);
        assert_eq!(records.len(), 2);
        assert_eq!(
            records[0].message,
            b"Subject: one\nFrom: a@x\n\nBody one\n".to_vec()
        );
        assert_eq!(records[0].offset, 0);
        assert_eq!(&input[records[1].offset as usize..][..5], b"From ");
        assert_eq!(records[0].len, records[1].offset);
        assert_eq!(records[1].offset + records[1].len, input.len() as u64);
        assert_eq!(
            records[1].message,
            b"Subject: two\nFrom: b@x\n\nBody two\n".to_vec()
        );
        assert_eq!(records[1].envelope.date, Some(1_704_067_201));
        // A record cut out by offset and length reads back the same.
        let cut = &input[records[1].offset as usize..(records[1].offset + records[1].len) as usize];
        assert_eq!(read_single_record(cut).unwrap().message, records[1].message);
    }

    #[test]
    fn unescapes_mboxrd_and_mboxo() {
        let input = b"From - Mon Jan 01 00:00:00 2024\n\
Subject: s\n\
X: y\n\
\n\
>From here\n\
>>From there\n\
> From quoted\n";
        let rd = read_all(input, Escaping::Mboxrd);
        assert_eq!(
            rd[0].message,
            b"Subject: s\nX: y\n\nFrom here\n>From there\n> From quoted\n".to_vec()
        );
        let o = read_all(input, Escaping::Mboxo);
        assert_eq!(
            o[0].message,
            b"Subject: s\nX: y\n\nFrom here\n>>From there\n> From quoted\n".to_vec()
        );
        let none = read_all(input, Escaping::None);
        assert!(none[0].message.windows(10).any(|w| w == b">From here"));
    }

    #[test]
    fn unescaped_from_lines_in_bodies_do_not_split() {
        let input = b"From - Mon Jan 01 00:00:00 2024\n\
Subject: s\n\
X: y\n\
\n\
From what I heard, this is fine.\n\
and continues here\n";
        let records = read_all(input, Escaping::Mboxrd);
        assert_eq!(records.len(), 1);
        assert!(records[0].message.ends_with(b"and continues here\n"));
    }

    #[test]
    fn skips_leading_garbage_and_survives_truncation() {
        let input = b"garbage line\n\
more\n\
From - Mon Jan 01 00:00:00 2024\n\
Subject: s\n\
X: y\n\
\n\
partial bo";
        let mut reader = MboxReader::new(&input[..]);
        let records: Vec<_> = reader.by_ref().map(Result::unwrap).collect();
        assert_eq!(reader.skipped_bytes(), 18);
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].offset, 18);
        assert_eq!(
            records[0].message,
            b"Subject: s\nX: y\n\npartial bo".to_vec()
        );
        assert!(read_all(b"", Escaping::Mboxrd).is_empty());
        assert!(read_all(b"no mbox at all\n", Escaping::Mboxrd).is_empty());
    }

    #[test]
    fn writer_round_trips_through_reader() {
        let messages: Vec<&[u8]> = vec![
            b"Subject: a\r\nX: 1\r\n\r\nFrom the start\r\n>From quoted\r\n\r\n",
            b"Subject: b\r\nX: 2\r\n\r\nno final newline",
            b"Subject: c\nX: 3\n\nlf only\n",
        ];
        let mut writer = MboxWriter::new(Vec::new());
        for (i, m) in messages.iter().enumerate() {
            let envelope = Envelope {
                sender: None,
                date: Some(1_700_000_000 + i as i64),
            };
            let state = MozillaState::from_flags(mazemail_types::Flags::SEEN, &[]);
            writer.write_message(&envelope, Some(&state), m).unwrap();
        }
        let file = writer.finish().unwrap();
        let records = read_all(&file, Escaping::Mboxrd);
        assert_eq!(records.len(), 3);
        for (i, record) in records.iter().enumerate() {
            let r = crate::reconstruct(&record.message);
            assert_eq!(r.mozilla.flags(), mazemail_types::Flags::SEEN);
            assert_eq!(record.envelope.date, Some(1_700_000_000 + i as i64));
            let expected = crate::reconstruct(messages[i]).bytes;
            if i == 1 {
                // A missing final line ending is added (as Thunderbird does).
                assert_eq!(r.bytes, [expected, b"\r\n".to_vec()].concat());
            } else {
                assert_eq!(r.bytes, expected);
            }
        }
    }
}
