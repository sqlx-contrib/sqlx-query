use std::marker::PhantomData;
use std::sync::LazyLock;

use regex::{Captures, Regex};

use crate::shift::shift_placeholders;
use crate::{OrderBy, QueryDialect, Value, Where};

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

/// Currently uninhabited — [`QueryComposer::build`] and
/// [`QueryComposer::render`] can't fail with the checks this crate
/// performs today (an unrecognized sentinel name is silently dropped, the
/// same as pgxquery). Kept as a real type, not [`std::convert::Infallible`],
/// so future validation (e.g. checking that a sentinel name in `sql` has a
/// corresponding builder call) can be added without an API break.
#[derive(Debug, thiserror::Error)]
pub enum Error {}

/// Splices a [`Where`] and an [`OrderBy`] into `/* query.<name> */`
/// sentinel comments in a static SQL template — the `pgx-contrib/pgxquery`
/// port. `sql` is never parsed structurally, only scanned once for its own
/// sentinel comments; it may contain any syntax the target driver accepts.
pub struct QueryComposer<DB: QueryDialect> {
    sql: &'static str,
    binds: Vec<Value>,
    where_by: Option<Where>,
    order_by: Option<OrderBy>,
    _dialect: PhantomData<fn() -> DB>,
}

impl<DB: QueryDialect> QueryComposer<DB> {
    pub fn new(sql: &'static str) -> Self {
        QueryComposer {
            sql,
            binds: Vec::new(),
            where_by: None,
            order_by: None,
            _dialect: PhantomData,
        }
    }

    /// A value for one of the base query's own placeholders, filled in
    /// declaration order: the first `bind` call is the base query's `$1`
    /// (or first `?`), the second is `$2`, and so on.
    pub fn bind(&mut self, value: impl Into<Value>) -> &mut Self {
        self.binds.push(value.into());
        self
    }

    /// Splices onto `/* query.where */`. Accepts anything that converts
    /// into [`Where`] — `sqlx-query-cel`'s `Filter`, a future keyset
    /// `Cursor`, or a `Where` built by hand. Its placeholders are shifted
    /// at [`build`](Self::build) time once the final offset (how many bind
    /// values precede it) is known.
    pub fn where_by(&mut self, filter: impl Into<Where>) -> &mut Self {
        self.where_by = Some(filter.into());
        self
    }

    /// Splices onto `/* query.order_by */`.
    pub fn order_by(&mut self, order_by: OrderBy) -> &mut Self {
        self.order_by = Some(order_by);
        self
    }

    /// Performs the sentinel splice and placeholder shift, returning the
    /// final SQL text and the flat bind-value list in the same order the
    /// final placeholders reference them: base binds, then the `where_by`
    /// value's own values — regardless of where either sentinel physically
    /// sits in `sql`. `order_by` never contributes values (see
    /// [`OrderBy::render`]), so it isn't part of that offset accounting.
    ///
    /// A sentinel whose value is absent, or whose value rendered to an
    /// empty string, is dropped entirely (including its captured
    /// connective/separator). A sentinel name that isn't `where` or
    /// `order_by` is also dropped — this matches pgxquery's handling of
    /// an unrecognized `query.<name>`.
    pub fn render(&self) -> (String, Vec<Value>) {
        let offset = self.binds.len();
        let (where_sql, where_values) = match &self.where_by {
            Some(where_by) if !where_by.sql().is_empty() => {
                let sql = if DB::positional() {
                    shift_placeholders(where_by.sql(), offset)
                } else {
                    where_by.sql().to_owned()
                };
                (sql, where_by.values().to_vec())
            }
            _ => (String::new(), Vec::new()),
        };

        let order_by_sql = self
            .order_by
            .as_ref()
            .map_or_else(String::new, OrderBy::render);

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

        let mut values = self.binds.clone();
        values.extend(where_values);

        (sql, values)
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
    /// Renders the query (see [`render`](Self::render)) and binds it into
    /// an executable `sqlx` query.
    ///
    /// The rendered SQL text is only known at `build` time (it depends on
    /// which values were spliced in), but `sqlx::query::Query` needs a
    /// `'static` SQL string. This leaks the rendered text (`Box::leak`) to
    /// get that `'static` lifetime honestly rather than faking it — each
    /// `build()` call leaks the size of its composed SQL. That's fine for
    /// the intended one-composer-per-request usage; do not call `build` in
    /// a loop expecting bounded memory.
    pub fn build(&self) -> Result<sqlx::query::Query<'static, DB, DB::Arguments<'static>>, Error> {
        let (sql, values) = self.render();
        let sql: &'static str = Box::leak(sql.into_boxed_str());

        let mut query = sqlx::query::<DB>(sql);
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
