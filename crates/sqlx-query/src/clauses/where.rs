use std::sync::Arc;

use sqlx::{AssertSqlSafe, SqlSafeStr, SqlStr};

use crate::string::shift_placeholders;
use crate::Value;

/// A `WHERE`-clause contribution: SQL text (no leading/trailing
/// connective, no `WHERE` keyword) plus the bind values its placeholders
/// reference, numbered locally as if it were the only thing in the query
/// (`$1`, `$2`, ... for positional dialects).
///
/// This is the concrete type [`QueryComposer::where_by`](crate::QueryComposer::where_by)
/// accepts. `sqlx-query` doesn't know about (and per DESIGN.md's
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
    ///
    /// Takes `impl AsRef<str>` rather than `impl Into<String>`: `Arc<str>`
    /// always copies its source bytes into a fresh allocation regardless
    /// (its layout differs from `String`'s, so the buffer can't be
    /// reused), so routing a `&str` literal through an intermediate
    /// owned `String` first would copy twice for no reason.
    pub fn new(sql: impl AsRef<str>) -> Self {
        let sql: Arc<str> = Arc::from(sql.as_ref());
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

    /// Combines two `WHERE`-shaped fragments with SQL `AND`, shifting
    /// `other`'s placeholders past `self`'s own value count so both can
    /// coexist as one fragment — e.g. a client filter and a pagination
    /// cursor's tuple comparison, neither of which should silently
    /// replace the other (see
    /// [`QueryComposer::cursor`](crate::QueryComposer::cursor)).
    pub fn and(self, other: WhereClause) -> WhereClause {
        let offset = self.values.len();
        let other_sql = shift_placeholders(other.sql().as_str(), offset);
        let sql = format!("({}) AND ({})", self.sql().as_str(), other_sql);

        self.values
            .iter()
            .chain(&other.values)
            .cloned()
            .fold(WhereClause::new(sql), WhereClause::bind)
    }

    /// Shifts this clause's placeholders past `offset` existing bind
    /// values. Always shifts — whether that's meaningful at all (only for
    /// positional `$N` dialects, not `?`-style ones) is the composer's
    /// call to make, not this type's; see
    /// [`QueryDialect::positional`](crate::QueryDialect::positional).
    ///
    /// `pub(crate)`, not `pub`: this is an offset-bookkeeping primitive
    /// specific to how [`QueryComposer`](crate::QueryComposer) splices
    /// fragments together — there's no reason for code outside this crate
    /// to reach for it directly, and keeping it internal means its only
    /// caller ([`QueryComposer::compose_where`](crate::QueryComposer))
    /// is one we already know always gates it behind
    /// [`QueryDialect::positional`](crate::QueryDialect::positional).
    pub(crate) fn shift(self, offset: usize) -> WhereClause {
        let sql = shift_placeholders(self.sql().as_str(), offset);
        self.values
            .into_iter()
            .fold(WhereClause::new(sql), WhereClause::bind)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn and_shifts_the_second_clauses_placeholders_past_the_first() {
        let a = WhereClause::new("status = $1").bind("ACTIVE");
        let b = WhereClause::new("rank > $1 AND id < $2")
            .bind(42i64)
            .bind(7i64);

        let combined = a.and(b);

        assert_eq!(
            combined.sql().as_str(),
            "(status = $1) AND (rank > $2 AND id < $3)"
        );
        assert_eq!(
            combined.values(),
            &[
                Value::String("ACTIVE".into()),
                Value::Int(42),
                Value::Int(7),
            ]
        );
    }

    #[test]
    fn and_with_no_values_on_either_side_needs_no_shift() {
        let a = WhereClause::new("deleted_at IS NULL");
        let b = WhereClause::new("archived = FALSE");

        let combined = a.and(b);

        assert_eq!(
            combined.sql().as_str(),
            "(deleted_at IS NULL) AND (archived = FALSE)"
        );
        assert!(combined.values().is_empty());
    }

    #[test]
    fn shift_moves_placeholders_past_the_offset() {
        let where_by = WhereClause::new("name = $1").bind("alice");
        let shifted = where_by.shift(2);

        assert_eq!(shifted.sql().as_str(), "name = $3");
        assert_eq!(shifted.values(), &[Value::String("alice".into())]);
    }
}
