use std::marker::PhantomData;
use std::sync::LazyLock;

use regex::{Captures, Regex};

use crate::{Cursor, OrderByClause, QueryDialect, Value, WhereClause};

/// Matches a sentinel comment of the form `/* query.<name> <suffix> */`,
/// capturing the name and the trailing connective/separator text
/// (`AND`, `OR`, `,`, or nothing) so it's preserved verbatim around the
/// substituted fragment.
///
/// This is the **name-first** convention used by `sqlc-gen-sqlx`'s
/// generated SQL (`/* query.where AND */`), not `pgx-contrib/pgxquery`'s
/// own connective-first convention (`/* AND query.where */`). Name-first
/// means there's no leading connective to capture, which is why this
/// pattern only has two groups.
static SENTINEL_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"/\*\s*\bquery\.(\w+)\b([^*]*?)\s*\*/").unwrap());

/// Errors [`QueryComposer::compose`]/[`QueryComposer::build`] can return.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// `order_by` was set to something that doesn't match the
    /// `OrderByClause` the [`Cursor`] passed to
    /// [`QueryComposer::with_cursor`] was built against — almost always
    /// means the client changed their sort between the request that
    /// issued the page token and the one using it.
    #[error("order_by doesn't match the order_by the cursor was built against")]
    CursorOrderByMismatch,
}

/// [`QueryComposer::compose`]'s output: SQL text with every sentinel
/// spliced in, plus the flat bind-value list in the same order the final
/// placeholders reference them — a complete, executable SQL statement
/// (text + parameters), which is what "statement" names here rather than
/// "fragment": unlike [`WhereClause`]/[`OrderByClause`], this isn't a
/// piece spliced into something larger, it's the whole thing.
#[derive(Debug, Clone, PartialEq)]
pub struct QueryStatement {
    sql: String,
    values: Vec<Value>,
}

impl QueryStatement {
    /// This statement's SQL text, sentinels already spliced in.
    #[must_use]
    pub fn sql(&self) -> &str {
        &self.sql
    }

    /// The bind values `sql()`'s placeholders reference, in declaration
    /// order.
    #[must_use]
    pub fn values(&self) -> &[Value] {
        &self.values
    }

    /// Consumes this statement into its two pieces — used internally by
    /// [`QueryComposer::build`], and a convenient way to destructure in
    /// tests: `let (sql, values) = composer.compose()?.into_parts();`.
    #[must_use]
    pub fn into_parts(self) -> (String, Vec<Value>) {
        (self.sql, self.values)
    }
}

/// Splices a [`WhereClause`] and an [`OrderByClause`] into
/// `/* query.<name> */` sentinel comments in a static SQL template — the
/// `pgx-contrib/pgxquery` port. `sql` is never parsed structurally, only
/// scanned once for its own sentinel comments; it may contain any syntax
/// the target driver accepts.
pub struct QueryComposer<DB: QueryDialect> {
    sql: &'static str,
    values: Vec<Value>,
    where_by: Vec<WhereClause>,
    order_by: Vec<OrderByClause>,
    cursor: Option<Cursor>,
    _dialect: PhantomData<fn() -> DB>,
}

impl<DB: QueryDialect> QueryComposer<DB> {
    /// Starts composing `sql` — a base query the caller already wrote,
    /// containing zero or more `/* query.where */`/`/* query.order_by */`
    /// sentinel comments. `sql` is never parsed structurally, so it may
    /// contain any syntax the target driver accepts; only its own
    /// sentinel comments are ever touched, and only once, by
    /// [`compose`](Self::compose)/[`build`](Self::build).
    #[must_use]
    pub fn new(sql: &'static str) -> Self {
        QueryComposer {
            sql,
            values: Vec::new(),
            where_by: Vec::new(),
            order_by: Vec::new(),
            cursor: None,
            _dialect: PhantomData,
        }
    }

    /// A value for one of the base query's own placeholders, filled in
    /// declaration order: the first `bind` call is the base query's `$1`
    /// (or first `?`), the second is `$2`, and so on.
    pub fn bind(&mut self, value: impl Into<Value>) -> &mut Self {
        self.values.push(value.into());
        self
    }

    /// Applies a keyset pagination [`Cursor`]. Its `to_where_clause()` is
    /// AND-ed in alongside anything passed to
    /// [`push_where`](Self::push_where) (a filter and pagination both
    /// apply — neither silently replaces the other). Its
    /// `to_order_by_clause()` doesn't have to be repeated: if
    /// [`push_order_by`](Self::push_order_by) is left unset, the cursor's is
    /// used directly; if it *is* set, [`compose`](Self::compose) checks the
    /// two match, since a mismatch almost always means the client's sort
    /// changed between the request that issued this cursor and this one.
    /// Only one cursor makes sense per query, so unlike `push_where`/
    /// `push_order_by` this doesn't accumulate — a second call replaces the
    /// first.
    pub fn with_cursor(&mut self, cursor: Cursor) -> &mut Self {
        self.cursor = Some(cursor);
        self
    }

    /// Splices onto `/* query.where */`. Accepts anything that converts
    /// into [`WhereClause`] — `sqlx-query-cel`'s `Filter`, a `WhereClause`
    /// built by hand, or anything else WHERE-shaped. Accumulates: each
    /// call ANDs its value onto whatever's already there (e.g. an
    /// always-present tenant-scoping condition, then a client-supplied
    /// filter) rather than replacing it, and if
    /// [`with_cursor`](Self::with_cursor) is also set, that's AND-ed in
    /// too — see [`compose`](Self::compose).
    pub fn push_where(&mut self, filter: impl Into<WhereClause>) -> &mut Self {
        self.where_by.push(filter.into());
        self
    }

    /// Splices onto `/* query.order_by */`. Accepts anything that converts
    /// into [`OrderByClause`]. Accumulates like
    /// [`push_where`](Self::push_where), but as tie-breakers in call
    /// order rather than an AND — "sort by the first call, **then** by
    /// the second" (see [`OrderByClause::then`]) — since ORDER BY is a
    /// sequence, not a boolean combination.
    pub fn push_order_by(&mut self, order_by: impl Into<OrderByClause>) -> &mut Self {
        self.order_by.push(order_by.into());
        self
    }

    /// Splices `compose_where`'s and `compose_order_by`'s output (both
    /// private helpers below) into their sentinels, returning the final
    /// SQL text and the flat bind-value list in the same order the final
    /// placeholders reference them: base binds, then `where_by`'s —
    /// regardless of where either sentinel physically sits in `sql`.
    ///
    /// A sentinel whose value is absent, or whose value rendered to an
    /// empty string, is dropped entirely (including its captured
    /// connective/separator). A sentinel name that isn't `where` or
    /// `order_by` is also dropped — this matches pgxquery's handling of
    /// an unrecognized `query.<name>`.
    ///
    /// # Errors
    ///
    /// [`Error::CursorOrderByMismatch`] if a cursor and an explicit
    /// `order_by` are both set and disagree.
    pub fn compose(&self) -> Result<QueryStatement, Error> {
        let order_by_sql = self.compose_order_by()?;
        let (where_by_sql, where_by_values) = self.compose_where();

        let sql = SENTINEL_RE
            .replace_all(self.sql, |caps: &Captures<'_>| {
                let value = match &caps[1] {
                    "where" => where_by_sql.as_str(),
                    "order_by" => order_by_sql.as_str(),
                    _ => "",
                };
                if value.is_empty() {
                    String::new()
                } else {
                    format!("{value}{}", &caps[2])
                }
            })
            .into_owned();

        let mut values = self.values.clone();
        values.extend(where_by_values);

        Ok(QueryStatement { sql, values })
    }

    /// Every accumulated `where_by` value plus the cursor's, all AND-ed
    /// together (dropping any that's absent) — a tenant scope, a
    /// client-supplied filter, and pagination can all apply at once; none
    /// silently replaces another. Empty ones are dropped; positional
    /// dialects (`$N`) additionally get their placeholders shifted past
    /// the base binds — non-positional ones (`?`) have no placeholder
    /// numbering to shift. Unlike `order_by`, this can't fail: `where_by`
    /// values are always AND-ed, never checked for equality against the
    /// cursor's.
    fn compose_where(&self) -> (String, Vec<Value>) {
        let where_by = self
            .where_by
            .iter()
            .cloned()
            .chain(self.cursor.as_ref().map(Cursor::to_where_clause))
            .reduce(WhereClause::and)
            .filter(|w| !w.sql().as_str().is_empty())
            .map(|w| {
                if DB::positional() {
                    w.shift(self.values.len())
                } else {
                    w
                }
            });

        let sql = where_by
            .as_ref()
            .map_or_else(String::new, |w| w.sql().as_str().to_owned());
        let values = where_by.map_or_else(Vec::new, |w| w.values().to_vec());
        (sql, values)
    }

    /// The accumulated `order_by` if any `push_order_by()` calls were
    /// made; otherwise the cursor's own. If both are present, they must
    /// match — the cursor's `order_by` only ever substitutes for a
    /// completely absent explicit one, it doesn't get appended as an
    /// extra tie-breaker onto an explicit `order_by` that IS present, and
    /// a mismatch almost always means the client's sort changed between
    /// the request that issued this cursor and this one. `order_by`
    /// never contributes bind values (see [`OrderByClause::sql`]).
    fn compose_order_by(&self) -> Result<String, Error> {
        // All accumulated order_by values, in call order, as tie-breakers —
        // "sort by the first push_order_by() call, then by the second", etc.
        let order_by: Option<OrderByClause> = self
            .order_by
            .clone()
            .into_iter()
            .reduce(OrderByClause::then);

        if let (Some(cursor), Some(order_by)) = (&self.cursor, &order_by) {
            if cursor.to_order_by_clause() != *order_by {
                return Err(Error::CursorOrderByMismatch);
            }
        }

        let order_by = order_by.or_else(|| self.cursor.as_ref().map(Cursor::to_order_by_clause));
        Ok(order_by.map_or_else(String::new, |order_by| order_by.sql().as_str().to_owned()))
    }
}

impl<DB> QueryComposer<DB>
where
    DB: QueryDialect,
    bool: sqlx::Type<DB> + sqlx::Encode<'static, DB>,
    i64: sqlx::Type<DB> + sqlx::Encode<'static, DB>,
    f64: sqlx::Type<DB> + sqlx::Encode<'static, DB>,
    String: sqlx::Type<DB> + sqlx::Encode<'static, DB>,
    chrono::DateTime<chrono::Utc>: sqlx::Type<DB> + sqlx::Encode<'static, DB>,
    Option<String>: sqlx::Type<DB> + sqlx::Encode<'static, DB>,
{
    /// Composes the query (see [`compose`](Self::compose)) and binds it
    /// into an executable `sqlx` query.
    ///
    /// The composed SQL text is only known at `build` time (it depends on
    /// which values were spliced in) — `sqlx::query()` accepts an owned
    /// `String` directly via [`sqlx::AssertSqlSafe`] (as of sqlx 0.9's
    /// `SqlSafeStr`), so unlike an earlier version of this method, nothing
    /// needs to be leaked to satisfy a `'static` bound.
    ///
    /// # Errors
    ///
    /// Whatever [`compose`](Self::compose) returns — this only binds what
    /// that produced.
    pub fn build(&self) -> Result<sqlx::query::Query<'static, DB, DB::Arguments>, Error> {
        let (sql, values) = self.compose()?.into_parts();

        let mut query = sqlx::query::<DB>(sqlx::AssertSqlSafe(sql));
        for value in values {
            query = match value {
                Value::Null => query.bind(None::<String>),
                Value::Bool(v) => query.bind(v),
                Value::Int(v) => query.bind(v),
                Value::Float(v) => query.bind(v),
                Value::String(v) => query.bind(v),
                Value::Timestamp(v) => query.bind(v),
            };
        }
        Ok(query)
    }
}
