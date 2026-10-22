//! Formatting of dates, sizes and counts for the message list and view.

use gpui::SharedString;
use time::{Date, OffsetDateTime, UtcOffset, macros::format_description};

/// Formats dates relative to "today" in the user's time zone, like Thunderbird:
/// time for today, weekday for the last week, day and month this year, full date
/// otherwise.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DateFormatter {
    offset: UtcOffset,
    today: Date,
}

impl DateFormatter {
    /// A formatter for "now" in the given UTC offset.
    pub fn new(offset: UtcOffset) -> Self {
        Self::at(OffsetDateTime::now_utc(), offset)
    }

    /// A formatter for a fixed "now", for tests.
    pub fn at(now: OffsetDateTime, offset: UtcOffset) -> Self {
        Self {
            offset,
            today: now.to_offset(offset).date(),
        }
    }

    /// The short form for list rows: `10:12`, `Wed`, `Oct 3`, `2024-10-03`.
    pub fn short(&self, date: OffsetDateTime) -> SharedString {
        let local = date.to_offset(self.offset);
        let days_ago = (self.today - local.date()).whole_days();
        let formatted = if days_ago == 0 {
            local.format(format_description!("[hour]:[minute]"))
        } else if (1..7).contains(&days_ago) {
            local.format(format_description!("[weekday repr:short]"))
        } else if local.year() == self.today.year() {
            local.format(format_description!("[month repr:short] [day padding:none]"))
        } else {
            local.format(format_description!("[year]-[month]-[day]"))
        };
        formatted.unwrap_or_default().into()
    }

    /// The long form for the message header: `Wed, 22 Oct 2026 10:12`.
    pub fn long(&self, date: OffsetDateTime) -> SharedString {
        date.to_offset(self.offset)
            .format(format_description!(
                "[weekday repr:short], [day padding:none] [month repr:short] [year] [hour]:[minute]"
            ))
            .unwrap_or_default()
            .into()
    }
}

/// `812 B`, `12 KB`, `3.4 MB`.
pub fn format_size(bytes: u64) -> SharedString {
    const KB: u64 = 1024;
    const MB: u64 = 1024 * KB;
    const GB: u64 = 1024 * MB;
    match bytes {
        0..KB => format!("{bytes} B"),
        KB..MB => format!("{} KB", bytes.div_ceil(KB)),
        MB..GB => format!("{:.1} MB", bytes as f64 / MB as f64),
        _ => format!("{:.1} GB", bytes as f64 / GB as f64),
    }
    .into()
}

/// `1,000,000`.
pub fn format_thousands(n: u64) -> SharedString {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (ix, c) in digits.chars().enumerate() {
        if ix > 0 && (digits.len() - ix).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out.into()
}

/// Up to two initials for an avatar: "Anna Example" → "AE".
pub fn initials(name: &str) -> SharedString {
    let letters: String = name
        .split_whitespace()
        .filter_map(|word| word.chars().find(|c| c.is_alphanumeric()))
        .take(2)
        .flat_map(char::to_uppercase)
        .collect();
    if letters.is_empty() {
        "?".into()
    } else {
        letters.into()
    }
}

#[cfg(test)]
mod tests {
    use time::macros::{datetime, offset};

    use super::*;

    #[test]
    fn short_dates_depend_on_age() {
        let f = DateFormatter::at(datetime!(2026-10-22 10:12 UTC), offset!(+2));
        assert_eq!(f.short(datetime!(2026-10-22 08:05 UTC)).as_ref(), "10:05");
        assert_eq!(f.short(datetime!(2026-10-21 08:05 UTC)).as_ref(), "Wed");
        assert_eq!(f.short(datetime!(2026-10-03 08:05 UTC)).as_ref(), "Oct 3");
        assert_eq!(
            f.short(datetime!(2024-10-03 08:05 UTC)).as_ref(),
            "2024-10-03"
        );
        // Local midnight decides "today", not UTC.
        assert_eq!(f.short(datetime!(2026-10-21 22:30 UTC)).as_ref(), "00:30");
        assert_eq!(
            f.long(datetime!(2026-10-22 08:12 UTC)).as_ref(),
            "Thu, 22 Oct 2026 10:12"
        );
    }

    #[test]
    fn sizes_and_counts() {
        assert_eq!(format_size(812).as_ref(), "812 B");
        assert_eq!(format_size(12_000).as_ref(), "12 KB");
        assert_eq!(format_size(3_565_158).as_ref(), "3.4 MB");
        assert_eq!(format_thousands(1_000_000).as_ref(), "1,000,000");
        assert_eq!(format_thousands(999).as_ref(), "999");
        assert_eq!(format_thousands(4_200).as_ref(), "4,200");
    }

    #[test]
    fn initials_from_names() {
        assert_eq!(initials("Anna Example").as_ref(), "AE");
        assert_eq!(initials("GitHub").as_ref(), "G");
        assert_eq!(initials("  ").as_ref(), "?");
        assert_eq!(initials("Tomás García López").as_ref(), "TG");
    }
}
