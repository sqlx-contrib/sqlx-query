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
    sql: String,
    values: Vec<Value>,
}

impl WhereClause {
    /// A `WhereClause` with no bind values — most hand-written filters
    /// (e.g. `"deleted_at IS NULL"`) don't reference any. Attach values
    /// with [`with_values`](Self::with_values) when the SQL has
    /// placeholders.
    pub fn new(sql: impl Into<String>) -> Self {
        WhereClause {
            sql: sql.into(),
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

    pub fn sql(&self) -> &str {
        &self.sql
    }
}
