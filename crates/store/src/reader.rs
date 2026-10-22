//! A small pool of read-only connections for cheap synchronous reads.

use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};

use rusqlite::Connection;

use crate::{Error, Result};

pub(crate) struct ReaderPool {
    idle: Mutex<PoolState>,
    available: Condvar,
}

struct PoolState {
    conns: Vec<Connection>,
    closed: bool,
}

impl ReaderPool {
    pub(crate) fn new(conns: Vec<Connection>) -> Arc<Self> {
        Arc::new(Self {
            idle: Mutex::new(PoolState {
                conns,
                closed: false,
            }),
            available: Condvar::new(),
        })
    }

    fn lock(&self) -> MutexGuard<'_, PoolState> {
        self.idle.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Waits for an idle connection.
    pub(crate) fn get(self: &Arc<Self>) -> Result<Reader> {
        let mut state = self.lock();
        loop {
            if state.closed {
                return Err(Error::Closed);
            }
            if let Some(conn) = state.conns.pop() {
                return Ok(Reader {
                    pool: Arc::clone(self),
                    conn: Some(conn),
                });
            }
            state = self
                .available
                .wait(state)
                .unwrap_or_else(PoisonError::into_inner);
        }
    }

    /// Closes idle connections; busy ones close when returned.
    pub(crate) fn close(&self) {
        let mut state = self.lock();
        state.closed = true;
        state.conns.clear();
        self.available.notify_all();
    }
}

/// A read-only connection borrowed from the pool; returned when dropped.
///
/// All queries come from the [`Queries`](crate::Queries) trait. Each call sees the
/// latest committed state; wrap several calls in [`Reader::snapshot`] to see one
/// consistent state.
pub struct Reader {
    pool: Arc<ReaderPool>,
    conn: Option<Connection>,
}

impl Reader {
    pub(crate) fn conn(&self) -> &Connection {
        // Only `None` during drop.
        self.conn
            .as_ref()
            .unwrap_or_else(|| unreachable!("reader used after drop"))
    }

    /// Runs `f` inside one read transaction, so every query in it sees the same
    /// snapshot of the database.
    pub fn snapshot<R>(&mut self, f: impl FnOnce(&Self) -> Result<R>) -> Result<R> {
        self.conn().execute_batch("BEGIN")?;
        let result = f(self);
        // A read transaction has nothing to roll back; end it either way.
        self.conn().execute_batch("COMMIT")?;
        result
    }
}

impl Drop for Reader {
    fn drop(&mut self) {
        if let Some(conn) = self.conn.take() {
            let mut state = self.pool.lock();
            if !state.closed {
                state.conns.push(conn);
                self.pool.available.notify_one();
            }
        }
    }
}

impl std::fmt::Debug for Reader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Reader").finish_non_exhaustive()
    }
}
