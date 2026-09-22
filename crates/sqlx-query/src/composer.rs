use std::marker::PhantomData;

use crate::lexer::{Placeholder, QueryLexer, Token};
use crate::{Cursor, CursorError, Error, OrderByClause, QueryDialect, Value, WhereClause};

/// What can go wrong while splicing, as opposed to what can go wrong with
/// a clause the composer was handed — those keep their own errors and
/// reach the caller through [`Error`](crate::Error).
#[derive(Debug, thiserror::Error)]
pub enum QueryComposerError {
    /// A query references more (or fewer) placeholders than the values
    /// bound for them.
    ///
    /// Checked rather than assumed because everything downstream depends
    /// on it: a fragment is shifted past the highest placeholder number
    /// ahead of it, and every placeholder is resolved by *index* into the
    /// value list. Those two agree only when the placeholders a query
    /// uses are exactly `$1..$n` for `n` bound values — so a base query
    /// that reaches for `$3` with two values bound would otherwise splice
    /// its fragment on top of a number already in use, and bind the wrong
    /// value to it rather than failing.
    #[error("query requires {required} value(s), but {bound} are bound")]
    BindMismatch { required: usize, bound: usize },

    /// A clause was set, but the base query has no slot to splice it
    /// into.
    ///
    /// The mirror of a slot with nothing to put in it, which is dropped
    /// rather than reported — the asymmetry is deliberate. An empty filter
    /// is the ordinary case and means "no filter"; a filter with nowhere
    /// to go means the query and the code disagree about what the query
    /// supports. Silently dropping it is how a
    /// [`Cursor`] ends up returning page one forever.
    #[error("a {name} clause was set, but the base query has no /* query.{name} */ slot")]
    MissingSlot { name: &'static str },
}

/// [`QueryComposer::compose`]'s output: SQL text with every slot
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
    /// This statement's SQL text, slots already spliced in.
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
}

/// Splices a [`WhereClause`] and an [`OrderByClause`] into
/// `/* query.<name> */` slots in a static SQL template — the
/// `pgx-contrib/pgxquery` port. `sql` is never parsed structurally, only
/// scanned once for its own slots; it may contain any syntax
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
    /// slots. `sql` is never parsed structurally, so it may
    /// contain any syntax the target driver accepts; only its own
    /// slots are ever touched, and only once, by
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
    /// declaration order: the first `bind_value` call is the base query's `$1`
    /// (or first `?`), the second is `$2`, and so on.
    pub fn bind_value(&mut self, value: impl Into<Value>) -> &mut Self {
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
    /// private helpers below) into their slots, returning the final
    /// SQL text and the bind-value list in the order the final
    /// placeholders reference them.
    ///
    /// Everything is numbered on the way through, whatever the dialect.
    /// A `?` can't be written down before its position in the finished
    /// statement is known — and a fragment's position isn't known until
    /// it has been spliced, which is *after* it was built. So for a
    /// `?`-style dialect the base query's `?` placeholders are numbered first
    /// (`?` → `$1`, `$2`, ... in textual order), everything is composed as
    /// if it were PostgreSQL, and the numbering is converted back to `?`
    /// in one final pass. That last pass is what makes the value list
    /// come out in *textual* order rather than base-then-fragments: a
    /// slot ahead of the base query's own `?` binds ahead of it too.
    ///
    /// The conversion emits one value per *reference*, not per value, so a
    /// number used twice (a [`Cursor`]'s tuple comparison reuses each
    /// boundary value) becomes two `?`s and two copies of the value —
    /// which is the thing a numbered placeholder can express and a bare
    /// bare `?` can't.
    ///
    /// For PostgreSQL both extra passes are skipped and the output is
    /// what it always was.
    ///
    /// A slot whose value is absent, or whose value rendered to an
    /// empty string, is dropped entirely (including its captured
    /// connective/separator). A slot name that isn't `where` or
    /// `order_by` is also dropped — this matches pgxquery's handling of
    /// an unrecognized `query.<name>`.
    ///
    /// # Errors
    ///
    /// [`CursorError::OrderByMismatch`] if a cursor and an explicit
    /// `order_by` are both set and disagree;
    /// [`PlaceholderError::Unsupported`](crate::PlaceholderError) for a
    /// `$N` in a base query whose dialect spells placeholders `?`;
    /// [`QueryComposerError::BindMismatch`] if the placeholders and the
    /// values don't correspond one-to-one;
    /// [`QueryComposerError::MissingSlot`] for a clause with nowhere to go.
    pub fn compose(&self) -> Result<QueryStatement, Error> {
        let syntax = DB::syntax();
        let lexer = QueryLexer::new(syntax);
        let tokens = lexer.scan(self.sql);

        let required = lexer.required_values(&tokens)?;
        if required != self.values.len() {
            return Err(QueryComposerError::BindMismatch {
                required,
                bound: self.values.len(),
            }
            .into());
        }

        let order_by_sql = self.compose_order_by()?;
        let (where_by_sql, where_by_values) = self.compose_where(required);

        let mut sql = String::with_capacity(self.sql.len());
        let mut last = 0;
        let mut question = 0;
        let mut spliced_where = false;
        let mut spliced_order_by = false;

        for token in &tokens {
            match *token {
                Token::Slot {
                    start,
                    end,
                    ref name,
                    ref suffix,
                } => {
                    let fragment = match name.as_str() {
                        "where" => {
                            spliced_where = true;
                            where_by_sql.as_str()
                        }
                        "order_by" => {
                            spliced_order_by = true;
                            order_by_sql.as_str()
                        }
                        _ => "",
                    };
                    sql.push_str(&self.sql[last..start]);
                    if !fragment.is_empty() {
                        sql.push_str(fragment);
                        sql.push_str(suffix);
                    }
                    last = end;
                }
                // Left alone for positional dialects, where `?` is an
                // operator (`jsonb ? text`) and never a placeholder.
                Token::Placeholder(Placeholder::Question { start, end })
                    if !syntax.placeholder.is_number() =>
                {
                    question += 1;
                    sql.push_str(&self.sql[last..start]);
                    sql.push('$');
                    sql.push_str(&question.to_string());
                    last = end;
                }
                Token::Placeholder(_) => {}
            }
        }
        sql.push_str(&self.sql[last..]);

        if !where_by_sql.is_empty() && !spliced_where {
            return Err(QueryComposerError::MissingSlot { name: "where" }.into());
        }
        // Only for an `order_by` the caller pushed. A cursor's own
        // `order_by` reaching a query with no slot for it is the
        // ordinary case of a base query that sorts statically — the sort
        // is already what the cursor was cut against, so there's nothing
        // to splice and nothing wrong. A `where` has no such out: a
        // boundary predicate can't have been written into the base query
        // ahead of time.
        if !order_by_sql.is_empty() && !self.order_by.is_empty() && !spliced_order_by {
            return Err(QueryComposerError::MissingSlot { name: "order_by" }.into());
        }

        let mut values = self.values.clone();
        values.extend(where_by_values);

        // The same check as above, now over the whole statement: the base
        // was verified before the fragments were shifted past it, and this
        // catches a hand-written fragment whose own numbering doesn't
        // match the values it carries.
        let required = lexer.max_placeholder_number(&sql);
        if required != values.len() {
            return Err(QueryComposerError::BindMismatch {
                required,
                bound: values.len(),
            }
            .into());
        }

        if syntax.placeholder.is_number() {
            return Ok(QueryStatement { sql, values });
        }
        Self::render_question_placeholders(&sql, &values, &lexer)
    }

    /// Converts a fully numbered statement back to the bare `?`
    /// placeholders a non-positional driver binds by position, emitting
    /// each one's value as it's encountered so the value list ends up in
    /// textual order.
    fn render_question_placeholders(
        sql: &str,
        values: &[Value],
        lexer: &QueryLexer,
    ) -> Result<QueryStatement, Error> {
        let mut out = String::with_capacity(sql.len());
        let mut rendered = Vec::with_capacity(values.len());
        let mut last = 0;

        for token in lexer.scan(sql) {
            if let Token::Placeholder(Placeholder::Number { start, end, number }) = token {
                let value = number
                    .checked_sub(1)
                    .and_then(|index| values.get(index))
                    .ok_or(QueryComposerError::BindMismatch {
                        required: number,
                        bound: values.len(),
                    })?;
                out.push_str(&sql[last..start]);
                out.push('?');
                rendered.push(value.clone());
                last = end;
            }
        }
        out.push_str(&sql[last..]);

        Ok(QueryStatement {
            sql: out,
            values: rendered,
        })
    }

    /// Every accumulated `where_by` value plus the cursor's, all AND-ed
    /// together (dropping any that's absent) — a tenant scope, a
    /// client-supplied filter, and pagination can all apply at once; none
    /// silently replaces another. Empty ones are dropped, and what's left
    /// is shifted past `offset`, the base query's own placeholders.
    /// Unlike `order_by`, this can't fail: `where_by` values are always
    /// AND-ed, never checked for equality against the cursor's.
    fn compose_where(&self, offset: usize) -> (String, Vec<Value>) {
        let where_by = self
            .where_by
            .iter()
            .cloned()
            .chain(self.cursor.as_ref().map(Cursor::to_where_clause))
            .reduce(WhereClause::and)
            .filter(|w| !w.sql().as_str().is_empty())
            .map(|w| w.shift(offset));

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
                return Err(CursorError::OrderByMismatch.into());
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
    Vec<u8>: sqlx::Type<DB> + sqlx::Encode<'static, DB>,
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
        let QueryStatement { sql, values } = self.compose()?;

        let mut query = sqlx::query::<DB>(sqlx::AssertSqlSafe(sql));
        for value in values {
            query = match value {
                Value::Null => query.bind(None::<String>),
                Value::Bool(v) => query.bind(v),
                Value::Int(v) => query.bind(v),
                Value::Float(v) => query.bind(v),
                Value::String(v) => query.bind(v),
                Value::Timestamp(v) => query.bind(v),
                Value::Bytes(v) => query.bind(v),
            };
        }
        Ok(query)
    }
}
