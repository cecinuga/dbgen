//! A minimal connection pool, for connectors whose driver clients hold one connection
//! each (PostgreSQL, MySQL, SQLite). A backend keeps a `Pool` and runs every call through
//! [`Pool::with`], which makes it safe to share across writer threads.

use std::sync::Mutex;

use super::DbResult;

type Open<C> = Box<dyn Fn() -> DbResult<C> + Send + Sync>;

/// Idle connections, reused across calls and opened on demand. At most as many
/// connections exist as calls run at the same time.
pub struct Pool<C> {
    idle: Mutex<Vec<C>>,
    open: Open<C>,
}

impl<C: Send> Pool<C> {
    /// An empty pool that opens connections with `open`.
    pub fn new(open: impl Fn() -> DbResult<C> + Send + Sync + 'static) -> Pool<C> {
        Pool { idle: Mutex::new(Vec::new()), open: Box::new(open) }
    }

    /// Runs `f` on an idle connection, opening one if none is idle. A connection whose call
    /// failed transiently may be broken, so it is dropped rather than reused.
    pub fn with<T>(&self, f: impl FnOnce(&mut C) -> DbResult<T>) -> DbResult<T> {
        let idle = self.idle.lock().expect("pool lock").pop();
        let mut conn = match idle {
            Some(c) => c,
            None => (self.open)()?,
        };
        let result = f(&mut conn);
        if !matches!(&result, Err(e) if e.is_transient()) {
            self.idle.lock().expect("pool lock").push(conn);
        }
        result
    }
}
