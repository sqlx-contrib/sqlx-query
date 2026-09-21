use std::marker::PhantomData;
use std::sync::LazyLock;

use regex::{Captures, Regex};

use crate::shift::shift_placeholders;
use crate::{Cursor, OrderByClause, QueryDialect, Value, WhereClause};

/// Matches a sentinel comment of the form `/* query.<name> <suffix> */`,
/// capturing the name and the trailing connective/separator text
/// (`AND`, `OR`, `,`, or nothing) so it's preserved verbatim around the
/// substituted fragment.
///
/// This is the **name-first** convention used by `sqlc-gen-sqlx`'s
/// generated SQL (`/* query.where AND */`), not `pgx-contrib/pgxquery`'s
/// own connective-first convention (`/* AND query.where */`) — see
/// DESIGN.md. Name-first means there's no leading connective to capture,
/// which is why this pattern only has two groups.
static SENTINEL_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"/\*\s*\bquery\.(\w+)\b([^*]*?)\s*\*/").unwrap());

#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// `order_by` was set to something that doesn't match the
    /// `OrderByClause` a `cursor()` was built against — almost always
    /// means the client changed their sort between the request that
    /// issued the page token and the one using it.
    #[error("order_by doesn't match the order_by the cursor was built against")]
    CursorOrderByMismatch,
}

/// Splices a [`WhereClause`] and an [`OrderByClause`] into
/// `/* query.<name> */` sentinel comments in a static SQL template — the
/// `pgx-contrib/pgxquery` port. `sql` is never parsed structurally, only
/// scanned once for its own sentinel comments; it may contain any syntax
/// the target driver accepts.
pub struct QueryComposer<DB: QueryDialect> {
    sql: &'static str,
    values: Vec<Value>,
    where_by: Option<WhereClause>,
    order_by: Option<OrderByClause>,
    cursor: Option<Cursor>,
    _dialect: PhantomData<fn() -> DB>,
}

impl<DB: QueryDialect> QueryComposer<DB> {
    pub fn new(sql: &'static str) -> Self {
        QueryComposer {
            sql,
            values: Vec::new(),
            where_by: None,
            order_by: None,
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

    /// Splices onto `/* query.where */`. Accepts anything that converts
    /// into [`WhereClause`] — `sqlx-query-cel`'s `Filter`, a `WhereClause`
    /// built by hand, or anything else WHERE-shaped. If [`cursor`](Self::cursor)
    /// is also set, both are AND-ed together — see [`render`](Self::render).
    pub fn where_by(&mut self, filter: impl Into<WhereClause>) -> &mut Self {
        self.where_by = Some(filter.into());
        self
    }

    /// Splices onto `/* query.order_by */`. Accepts anything that converts
    /// into [`OrderByClause`], mirroring [`where_by`](Self::where_by) —
    /// today that's only `OrderByClause` itself, but this keeps the two
    /// builder methods symmetric without committing to that forever.
    pub fn order_by(&mut self, order_by: impl Into<OrderByClause>) -> &mut Self {
        self.order_by = Some(order_by.into());
        self
    }

    /// Applies a keyset pagination [`Cursor`]. Its `where_by()` is AND-ed
    /// with any value passed to [`where_by`](Self::where_by) (a filter and
    /// pagination both apply — one must not silently replace the other).
    /// Its `order_by()` doesn't have to be repeated: if
    /// [`order_by`](Self::order_by) is left unset, the cursor's is used
    /// directly; if it *is* set, [`render`](Self::render) checks the two
    /// match, since a mismatch almost always means the client's sort
    /// changed between the request that issued this cursor and this one.
    pub fn cursor(&mut self, cursor: Cursor) -> &mut Self {
        self.cursor = Some(cursor);
        self
    }

    /// Performs the sentinel splice and placeholder shift, returning the
    /// final SQL text and the flat bind-value list in the same order the
    /// final placeholders reference them: base binds, then the effective
    /// `where_by` value's own values — regardless of where either
    /// sentinel physically sits in `sql`. `order_by` never contributes
    /// values (see [`OrderByClause::sql`]), so it isn't part of that
    /// offset accounting.
    ///
    /// A sentinel whose value is absent, or whose value rendered to an
    /// empty string, is dropped entirely (including its captured
    /// connective/separator). A sentinel name that isn't `where` or
    /// `order_by` is also dropped — this matches pgxquery's handling of
    /// an unrecognized `query.<name>`.
    pub fn render(&self) -> Result<(String, Vec<Value>), Error> {
        if let (Some(cursor), Some(order_by)) = (&self.cursor, &self.order_by) {
            if cursor.order_by() != *order_by {
                return Err(Error::CursorOrderByMismatch);
            }
        }

        let cursor_where_by = self.cursor.as_ref().map(Cursor::where_by);
        let where_by = match (self.where_by.clone(), cursor_where_by) {
            (Some(filter), Some(cursor)) => Some(merge_where_by(filter, cursor)),
            (Some(filter), None) => Some(filter),
            (None, Some(cursor)) => Some(cursor),
            (None, None) => None,
        };
        let order_by = self
            .order_by
            .clone()
            .or_else(|| self.cursor.as_ref().map(Cursor::order_by));

        let offset = self.values.len();
        let (where_sql, where_values) = match &where_by {
            Some(where_by) if !where_by.sql().as_str().is_empty() => {
                let sql = if DB::positional() {
                    shift_placeholders(where_by.sql().as_str(), offset)
                } else {
                    where_by.sql().as_str().to_owned()
                };
                (sql, where_by.values().to_vec())
            }
            _ => (String::new(), Vec::new()),
        };

        let order_by_sql =
            order_by.map_or_else(String::new, |order_by| order_by.sql().as_str().to_owned());

        let sql = SENTINEL_RE
            .replace_all(self.sql, |caps: &Captures<'_>| {
                let value = match &caps[1] {
                    "where" => where_sql.as_str(),
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
        values.extend(where_values);

        Ok((sql, values))
    }
}

/// ANDs a client filter together with a cursor's tuple comparison, since
/// pagination must not silently drop the filter it's layered on top of.
/// Built entirely from `WhereClause`'s existing public API (`sql()`,
/// `values()`, `new()`, `bind()`) — `WhereClause` itself doesn't know
/// anything about this.
fn merge_where_by(filter: WhereClause, cursor: WhereClause) -> WhereClause {
    let offset = filter.values().len();
    let cursor_sql = shift_placeholders(cursor.sql().as_str(), offset);
    let sql = format!("({}) AND ({})", filter.sql().as_str(), cursor_sql);

    filter
        .values()
        .iter()
        .chain(cursor.values())
        .cloned()
        .fold(WhereClause::new(sql), WhereClause::bind)
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
    /// Renders the query (see [`render`](Self::render)) and binds it into
    /// an executable `sqlx` query.
    ///
    /// The rendered SQL text is only known at `build` time (it depends on
    /// which values were spliced in) — `sqlx::query()` accepts an owned
    /// `String` directly via [`sqlx::AssertSqlSafe`] (as of sqlx 0.9's
    /// `SqlSafeStr`), so unlike an earlier version of this method, nothing
    /// needs to be leaked to satisfy a `'static` bound.
    pub fn build(&self) -> Result<sqlx::query::Query<'static, DB, DB::Arguments>, Error> {
        let (sql, values) = self.render()?;

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
