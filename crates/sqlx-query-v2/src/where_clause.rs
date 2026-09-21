use std::sync::Arc;

use sqlx::{AssertSqlSafe, SqlSafeStr, SqlStr};

use crate::Value;

/// A `WHERE`-clause contribution: SQL text (no leading/trailing
/// connective, no `WHERE` keyword) plus the bind values its placeholders
/// reference, numbered locally as if it were the only thing in the query
/// (`$1`, `$2`, ... for positional dialects).
///
/// This is the concrete type [`QueryComposer::where_by`](crate::QueryComposer::where_by)
/// accepts. `sqlx-query-v2` doesn't know about (and per DESIGN.md's
/// dependency direction, must never depend on) `sqlx-query-cel`'s
/// `Filter` — so `Filter`, or any other future `WHERE`-shaped value (e.g.
/// a keyset `Cursor`), converts *into* `WhereClause` via [`Into`], rather than
/// `WhereClause` reaching out to know about them.
#[derive(Debug, Clone, PartialEq)]
pub struct WhereClause {
    sql: SqlStr,
    values: Vec<Value>,
}

impl WhereClause {
    /// A `WhereClause` with no bind values — most hand-written filters
    /// (e.g. `"deleted_at IS NULL"`) don't reference any. Attach values
    /// with [`bind`](Self::bind) when the SQL has placeholders.
    ///
    /// `sql` is stored as an `Arc<str>`-backed [`SqlStr`] up front, so
    /// every later [`sql()`](Self::sql) call is just a refcount bump, not
    /// a copy — see [`sql()`](Self::sql)'s doc for why that matters.
    pub fn new(sql: impl Into<String>) -> Self {
        let sql: Arc<str> = Arc::from(sql.into());
        WhereClause {
            sql: AssertSqlSafe(sql).into_sql_str(),
            values: Vec::new(),
        }
    }

    /// A value for one of this clause's own placeholders, filled in
    /// declaration order — mirrors [`QueryComposer::bind`](crate::QueryComposer::bind).
    pub fn bind(mut self, value: impl Into<Value>) -> Self {
        self.values.push(value.into());
        self
    }

    pub fn values(&self) -> &[Value] {
        &self.values
    }

    /// This clause's SQL text. Matches
    /// [`OrderByClause::sql`](crate::OrderByClause::sql)'s return type —
    /// both clause types answer "what's your SQL text?" as [`SqlStr`], the
    /// same type `sqlx::query()` itself wants. Unlike `OrderByClause`'s
    /// (computed fresh from `terms` on every call), this one is a cheap
    /// clone: `new()` stores the text `Arc`-backed, and `SqlStr::clone`
    /// is just a refcount bump for the `Arc` variant.
    pub fn sql(&self) -> SqlStr {
        self.sql.clone()
    }
}
