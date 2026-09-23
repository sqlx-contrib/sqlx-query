use std::sync::Arc;

use sqlx::{AssertSqlSafe, SqlSafeStr, SqlStr};

use crate::lexer::QueryLexer;
use crate::Value;

/// A `WHERE`-clause contribution: SQL text (no leading/trailing
/// connective, no `WHERE` keyword) plus the bind values its placeholders
/// reference, numbered locally as if it were the only thing in the query
/// (`$1`, `$2`, ... for positional dialects).
///
/// This is the concrete type [`QueryComposer::push_where`](crate::QueryComposer::push_where)
/// accepts. `sqlx-query` doesn't know about (and must never depend on,
/// to keep the dependency direction one-way) `sqlx-query-cel`'s
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
    /// with [`bind_value`](Self::bind_value) when the SQL has placeholders.
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
    /// declaration order — mirrors [`QueryComposer::bind_value`](crate::QueryComposer::bind_value).
    #[must_use]
    pub fn bind_value(mut self, value: impl Into<Value>) -> Self {
        self.values.push(value.into());
        self
    }

    /// The bind values `sql()`'s placeholders reference, in declaration
    /// order.
    #[must_use]
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
    #[must_use]
    pub fn sql(&self) -> SqlStr {
        self.sql.clone()
    }

    /// Combines two `WHERE`-shaped fragments with SQL `AND`, shifting
    /// `other`'s placeholders past `self`'s own so both can coexist as one
    /// fragment — e.g. a client filter and a pagination cursor's tuple
    /// comparison, neither of which should silently replace the other (see
    /// [`QueryComposer::with_cursor`](crate::QueryComposer::with_cursor)).
    ///
    /// Shifts past `self`'s *highest placeholder number*, not its value
    /// count. The two agree for any well-formed fragment, and where they
    /// disagree the number is the one that matters: a fragment may
    /// reference one value twice (a [`Cursor`](crate::Cursor)'s tuple
    /// comparison does exactly that), and shifting by the count would then
    /// land `other` on top of a number `self` is still using.
    #[must_use]
    pub fn and(self, other: WhereClause) -> WhereClause {
        self.combine(other, "AND")
    }

    /// Combines two `WHERE`-shaped fragments with SQL `OR`, shifting
    /// `other`'s placeholders past `self`'s own exactly as
    /// [`and`](Self::and) does.
    ///
    /// Both sides are parenthesised, so there's no precedence to reason
    /// about when mixing this with `and`.
    #[must_use]
    pub fn or(self, other: WhereClause) -> WhereClause {
        self.combine(other, "OR")
    }

    fn combine(self, other: WhereClause, connective: &str) -> WhereClause {
        let lexer = QueryLexer::standard();
        let offset = lexer.max_placeholder_number(self.sql().as_str());
        let other_sql = lexer.shift_placeholder_numbers(other.sql().as_str(), offset);
        let sql = format!("({}) {connective} ({})", self.sql().as_str(), other_sql);

        self.values
            .into_iter()
            .chain(other.values)
            .fold(WhereClause::new(sql), WhereClause::bind_value)
    }

    /// Shifts this clause's placeholders past `offset` placeholders that
    /// already exist ahead of it.
    ///
    /// Unconditional, for every dialect: a fragment is always numbered
    /// (`$1`, `$2`, ...) no matter where it's going, because a `?` can't
    /// be written down before its position in the final statement is
    /// known. Converting back to `?` happens once, at the end, in
    /// [`QueryComposer::compose`](crate::QueryComposer::compose).
    ///
    /// `pub(crate)`, not `pub`: this is an offset-bookkeeping primitive
    /// specific to how [`QueryComposer`](crate::QueryComposer) splices
    /// fragments together — there's no reason for code outside this crate
    /// to reach for it directly.
    pub(crate) fn shift(self, offset: usize) -> WhereClause {
        let sql = QueryLexer::standard().shift_placeholder_numbers(self.sql().as_str(), offset);
        self.values
            .into_iter()
            .fold(WhereClause::new(sql), WhereClause::bind_value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn and_shifts_the_second_clauses_placeholders_past_the_first() {
        let a = WhereClause::new("status = $1").bind_value("ACTIVE");
        let b = WhereClause::new("rank > $1 AND id < $2")
            .bind_value(42i64)
            .bind_value(7i64);

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
    fn or_combines_like_and_but_with_or() {
        let a = WhereClause::new("status = $1").bind_value("ACTIVE");
        let b = WhereClause::new("rank > $1").bind_value(42i64);

        let combined = a.or(b);

        assert_eq!(combined.sql().as_str(), "(status = $1) OR (rank > $2)");
        assert_eq!(
            combined.values(),
            &[Value::String("ACTIVE".into()), Value::Int(42)]
        );
    }

    #[test]
    fn shift_moves_placeholders_past_the_offset() {
        let where_by = WhereClause::new("name = $1").bind_value("alice");
        let shifted = where_by.shift(2);

        assert_eq!(shifted.sql().as_str(), "name = $3");
        assert_eq!(shifted.values(), &[Value::String("alice".into())]);
    }
}
