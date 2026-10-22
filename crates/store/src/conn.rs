//! Opening SQLCipher connections: key first, then verification, then pragmas.

use std::fs::File;
use std::io::Read;
use std::path::Path;
use std::time::Duration;

use rusqlite::{Connection, ErrorCode, OpenFlags};
use vault::SecretString;
use zeroize::Zeroizing;

use crate::{Error, Result};

/// What a connection is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Access {
    /// Read-write; creates the file if it doesn't exist.
    ReadWrite,
    /// `query_only`, never creates the file.
    ReadOnly,
}

const SQLITE_HEADER: &[u8; 16] = b"SQLite format 3\0";

/// Opens `path` with the SQLCipher raw key and the store's pragmas.
///
/// The key is applied before anything else touches the file and checked right away by
/// reading the schema; an existing file that doesn't decrypt is [`Error::WrongKey`] (or
/// [`Error::PlaintextDatabase`]), never a fresh database.
pub(crate) fn open(
    path: &Path,
    key: &SecretString,
    access: Access,
    cache_kib: u32,
) -> Result<Connection> {
    if file_has_plaintext_header(path)? {
        return Err(Error::PlaintextDatabase {
            path: path.to_owned(),
        });
    }

    let flags = match access {
        Access::ReadWrite => {
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_CREATE
                | OpenFlags::SQLITE_OPEN_NO_MUTEX
        }
        Access::ReadOnly => OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    };
    let conn = Connection::open_with_flags(path, flags)?;
    apply_key(&conn, key)?;
    verify_key(&conn, path)?;

    conn.busy_timeout(Duration::from_secs(5))?;
    // SQLCipher can't use memory-mapped I/O; make that explicit.
    conn.execute_batch(&format!(
        "PRAGMA foreign_keys = ON;
         PRAGMA temp_store = MEMORY;
         PRAGMA mmap_size = 0;
         PRAGMA cache_size = -{cache_kib};"
    ))?;
    match access {
        Access::ReadWrite => {
            let mode: String = conn.query_row("PRAGMA journal_mode = WAL", [], |row| row.get(0))?;
            if !mode.eq_ignore_ascii_case("wal") {
                tracing::warn!(%mode, "SQLite refused WAL mode");
            }
            conn.execute_batch("PRAGMA synchronous = NORMAL;")?;
        }
        Access::ReadOnly => conn.execute_batch("PRAGMA query_only = ON;")?,
    }
    conn.set_prepared_statement_cache_capacity(128);
    rusqlite::vtab::array::load_module(&conn)?;
    Ok(conn)
}

/// `PRAGMA key = "x'…'"`: a raw 256-bit key, so there's no PBKDF2 cost per connection.
pub(crate) fn apply_key(conn: &Connection, key: &SecretString) -> Result<()> {
    let statement = Zeroizing::new(format!("PRAGMA key = \"{}\";", key.expose_secret()));
    conn.execute_batch(&statement)?;
    Ok(())
}

/// Reads the schema; with the wrong key SQLCipher reports `SQLITE_NOTADB` here.
fn verify_key(conn: &Connection, path: &Path) -> Result<()> {
    match conn.query_row("SELECT count(*) FROM sqlite_master", [], |row| {
        row.get::<_, i64>(0)
    }) {
        Ok(_) => Ok(()),
        Err(rusqlite::Error::SqliteFailure(err, _)) if err.code == ErrorCode::NotADatabase => {
            Err(Error::WrongKey {
                path: path.to_owned(),
            })
        }
        Err(err) => Err(err.into()),
    }
}

/// True if `path` exists and starts with the plaintext SQLite header.
fn file_has_plaintext_header(path: &Path) -> Result<bool> {
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(err) => return Err(Error::io("reading the database header")(err)),
    };
    let mut header = [0u8; 16];
    match file.read_exact(&mut header) {
        Ok(()) => Ok(&header == SQLITE_HEADER),
        Err(err) if err.kind() == std::io::ErrorKind::UnexpectedEof => Ok(false),
        Err(err) => Err(Error::io("reading the database header")(err)),
    }
}

/// The SQLCipher crypto provider (`commoncrypto`, `openssl`, …), for diagnostics.
pub(crate) fn cipher_provider(conn: &Connection) -> Result<String> {
    Ok(conn.query_row("PRAGMA cipher_provider", [], |row| row.get(0))?)
}
