//! The personal spelling dictionary ("Add to Dictionary" in the composer, schema v12).

use rusqlite::{Connection, params};

use crate::model::UnixTime;
use crate::{Error, Result, WriteTx};

/// A word as stored: trimmed, one word (no whitespace), at most 100 characters.
fn normalize(word: &str) -> Option<&str> {
    let word = word.trim();
    (!word.is_empty() && !word.contains(char::is_whitespace) && word.chars().count() <= 100)
        .then_some(word)
}

pub(crate) fn spelling_words(conn: &Connection) -> Result<Vec<String>> {
    let mut stmt = conn.prepare_cached("SELECT word FROM spelling_word ORDER BY word")?;
    let words = stmt
        .query_map([], |row| row.get(0))?
        .collect::<rusqlite::Result<Vec<String>>>()?;
    Ok(words)
}

impl WriteTx<'_> {
    /// Adds `word` to the personal spelling dictionary (idempotent). Words are kept as
    /// written: case matters the way the spell checker defines it.
    pub fn add_spelling_word(&mut self, word: &str, now: UnixTime) -> Result<()> {
        let word = normalize(word).ok_or(Error::Invalid("not a single word"))?;
        self.conn
            .prepare_cached(
                "INSERT INTO spelling_word (word, added) VALUES (?1, ?2)
                 ON CONFLICT (word) DO NOTHING",
            )?
            .execute(params![word, now])?;
        Ok(())
    }

    /// Removes `word` from the personal spelling dictionary; returns whether it was
    /// there.
    pub fn remove_spelling_word(&mut self, word: &str) -> Result<bool> {
        let Some(word) = normalize(word) else {
            return Ok(false);
        };
        let removed = self
            .conn
            .prepare_cached("DELETE FROM spelling_word WHERE word = ?1")?
            .execute([word])?;
        Ok(removed > 0)
    }
}
