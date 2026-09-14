use std::collections::{HashMap, HashSet};
use std::fmt;
use std::ops::{ControlFlow, Range};

use sqlparser::ast::{
    BinaryOperator, Expr, GroupByExpr, Ident, LimitClause, OrderBy, OrderByExpr, OrderByKind,
    OrderByOptions, OrderBySort, Query, SetExpr, Statement, Value, VisitMut, visit_expressions_mut,
};
use sqlparser::dialect::Dialect;
use sqlparser::parser::{Parser, ParserError};
use sqlparser::tokenizer::Token;
use sqlx::query::{Query as SqlxQuery, QueryAs};
use sqlx::{Arguments, AssertSqlSafe, Encode, FromRow, Type};

/// The two things a rewrite needs from a driver: how to read its SQL, and how
/// to write a placeholder back out.
///
/// This is a supertrait of [`sqlx::Database`] rather than a parallel hierarchy,
/// so `QueryWriter<Postgres>` names the same `Postgres` the rest of your
/// queries do and no adapter type stands between them.
///
/// # Implementing it for a driver this crate does not ship
///
/// Nothing stops you. What you are claiming by doing so is that
/// [`parser`](Self::parser) accepts the same grammar the driver will actually
/// run, and that [`placeholder`](Self::placeholder) and
/// [`positional`](Self::positional) agree with how it binds. Get those wrong
/// and the rewrite produces SQL that parses here and means something else
/// there, which is not a failure any test in this crate can catch for you.
pub trait Syntax: sqlx::Database<Arguments: sqlx::IntoArguments<Self>> {
    /// The grammar the base query and its fragments are parsed with.
    fn parser() -> &'static dyn Dialect;

    /// Renders the placeholder that binds the `index`th value, counting from
    /// zero.
    ///
    /// It has to *name* that value rather than merely occupy a position, so
    /// that a fragment spliced into the middle of a query does not disturb
    /// what the placeholders after it bind. PostgreSQL's `$N` and SQLite's
    /// `?N` both do. MySQL's bare `?` does not, which is why this crate does
    /// not support it: the values would have to be reordered to match, and
    /// nothing in the SQL would show that it had happened.
    fn placeholder(index: usize) -> String;
}

#[cfg(feature = "postgres")]
mod postgres {
    use super::{Dialect, Syntax};
    use sqlparser::dialect::PostgreSqlDialect;

    static DIALECT: PostgreSqlDialect = PostgreSqlDialect {};

    impl Syntax for sqlx::Postgres {
        fn parser() -> &'static dyn Dialect {
            &DIALECT
        }

        fn placeholder(index: usize) -> String {
            format!("${}", index + 1)
        }
    }
}

#[cfg(feature = "sqlite")]
mod sqlite {
    use super::{Dialect, Syntax};
    use sqlparser::dialect::SQLiteDialect;

    static DIALECT: SQLiteDialect = SQLiteDialect {};

    impl Syntax for sqlx::Sqlite {
        fn parser() -> &'static dyn Dialect {
            &DIALECT
        }

        // `?NNN`, not bare `?`. SQLite is the only one of the three whose
        // placeholder can be both a question mark and numbered, which puts it
        // on the same footing as PostgreSQL: a placeholder names the value it
        // wants, so a fragment spliced ahead of it does not disturb it and
        // nothing has to be replayed in a different order.
        fn placeholder(index: usize) -> String {
            format!("?{}", index + 1)
        }
    }
}

/// A placeholder that has been numbered but not yet written in the driver's
/// own form.
///
/// Spelled distinctively rather than as `$4`, because the last pass finds it in
/// rendered SQL and a string literal is allowed to contain `$4`. The numbering
/// is the idea; this is only how it is written down in between.
const NUMBERED: &str = "$__sqlxq_";

/// The same, before the numbers are known: one per placeholder node, so they
/// can be told apart while their order is being worked out.
const PENDING: &str = "$__sqlxqt_";

const SUFFIX: &str = "__";

/// Renders a node so its placeholders can be read in the order they print.
///
/// Display order is the order the database sees, and the only authority on it:
/// sqlparser's `Select` prints `top` before `distinct` or after it depending on
/// a runtime flag, so no traversal of the tree can stand in for this.
trait Render {
    fn render(&self) -> String;
}

impl Render for Statement {
    fn render(&self) -> String {
        self.to_string()
    }
}

impl Render for Expr {
    fn render(&self) -> String {
        self.to_string()
    }
}

impl Render for Vec<OrderByExpr> {
    fn render(&self) -> String {
        self.iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// Adds filters and ordering to a query you already wrote.
///
/// The query stays a query: there are no markers in it, nothing to escape, and
/// nothing a formatter or `EXPLAIN` will choke on. Fragments are parsed and
/// grafted onto its syntax tree, so the result is a statement this crate
/// assembled rather than one it concatenated.
///
/// ```
/// # #[cfg(feature = "postgres")] {
/// use sqlx::Postgres;
/// use sqlx_query::QueryWriter;
///
/// let mut writer = QueryWriter::<Postgres>::new(
///     "SELECT id, name, role FROM users WHERE tenant_id = $1 ORDER BY id",
/// )?;
///
/// writer.bind(7_i64).and_where("role = 'admin'").order_by("name asc");
///
/// assert_eq!(
///     writer.sql()?,
///     "SELECT id, name, role FROM users WHERE tenant_id = $1 AND role = 'admin' \
///      ORDER BY name ASC, id",
/// );
/// # }
/// # Ok::<_, sqlx_query::Error>(())
/// ```
///
/// # Fragments are checked, not trusted
///
/// A fragment arrives as text, usually from something that compiled it out of
/// a request. It is parsed with the same syntax as the query, and it has to
/// come out as exactly one expression: `role = 'admin'` does, and
/// `role = 'admin'; DROP TABLE users` does not, because the statement after
/// the expression is left over. That leftover is [`Error::Trailing`], and it
/// is refused here rather than sent.
///
/// This is a check on *shape*, not on meaning. A fragment is still SQL, and
/// `role = 'admin' OR 1=1` is a perfectly well-formed expression. Build
/// fragments from an allowlist of columns; do not paste user input into one.
///
/// # Errors surface at the end
///
/// The methods that take a fragment return `&mut Self` so a chain reads in one
/// line. A fragment that does not parse is remembered and returned from
/// [`sql`](Self::sql), [`build`](Self::build) or [`build_as`](Self::build_as)
/// -- the first failure, every time it is asked.
pub struct QueryWriter<DB: Syntax> {
    statement: Statement,
    filters: Vec<Expr>,
    order_by: Vec<OrderByExpr>,
    limit: Option<u64>,

    // How many values have been claimed so far, and so where the next
    // fragment's own numbering starts from.
    arity: usize,

    arguments: DB::Arguments,
    failure: Option<Error>,
}

impl<DB: Syntax> QueryWriter<DB> {
    /// Parses the query to rewrite.
    ///
    /// # Errors
    ///
    /// [`Error::Query`] if the SQL does not parse in this driver's syntax,
    /// and [`Error::NotQuery`] if it parses as something other than a query --
    /// an `INSERT` has no `WHERE` for a filter to join.
    pub fn new(sql: &str) -> Result<Self, Error> {
        let mut statements = Parser::parse_sql(DB::parser(), sql).map_err(Error::Query)?;

        // A trailing semicolon parses to one statement, so this rejects a
        // genuine second one rather than punctuation.
        if statements.len() != 1 {
            return Err(Error::NotQuery);
        }
        let mut statement = statements.remove(0);
        if !matches!(statement, Statement::Query(_)) {
            return Err(Error::NotQuery);
        }

        // The base query's placeholders are claimed first, so they are
        // numbered from zero and every fragment follows them.
        let arity = Self::number(&mut statement, 0);

        Ok(Self {
            statement,
            filters: Vec::new(),
            order_by: Vec::new(),
            limit: None,
            arity,
            arguments: DB::Arguments::default(),
            failure: None,
        })
    }

    /// Binds the next value.
    ///
    /// Values are given in the order the placeholders claim them: the base
    /// query's first, then each fragment's, in the order the fragments were
    /// added. A fragment numbers its own placeholders from `$1`, and they are
    /// renumbered to follow whatever came before.
    ///
    /// The order they are given in is the order they are sent in: every
    /// placeholder names the value it wants, so nothing has to be rearranged
    /// to match where it ended up.
    pub fn bind<'t, T>(&mut self, value: T) -> &mut Self
    where
        T: Encode<'t, DB> + Type<DB>,
    {
        if let Err(error) = self.arguments.add(value) {
            // Raised when a value cannot be encoded at all -- out of range for
            // the wire format, say. Nothing later can fix it, and it reads
            // better next to the fragment errors than as a separate result on
            // every `bind`.
            self.fail(Error::Encode(error.to_string()));
        }
        self
    }

    /// Joins a fragment onto the query's `WHERE` with `AND`.
    ///
    /// Called more than once, the fragments are `AND`ed together. An existing
    /// `WHERE` is kept and joined the same way -- this adds a condition, it
    /// never replaces one.
    #[doc(alias = "where")]
    pub fn and_where(&mut self, fragment: &str) -> &mut Self {
        match self.parse(fragment, Parser::parse_expr) {
            Ok(expr) => self.filters.push(expr),
            Err(error) => self.fail(error),
        }
        self
    }

    /// Puts a fragment in front of the query's `ORDER BY`.
    ///
    /// What the query already ordered by is kept, and moves behind the
    /// fragment as a tiebreaker -- a base `ORDER BY id` is usually there to
    /// make the order total, which it still does from second place. A column
    /// named by both is only ordered by once, at the position the fragment
    /// gave it.
    pub fn order_by(&mut self, fragment: &str) -> &mut Self {
        match self.parse(fragment, |parser| {
            parser.parse_comma_separated(Parser::parse_order_by_expr)
        }) {
            Ok(exprs) => self.order_by.extend(exprs),
            Err(error) => self.fail(error),
        }
        self
    }

    /// Sets `LIMIT`, replacing the query's own.
    ///
    /// The row count is written into the statement rather than bound, because
    /// it came from this program and not from a request -- and an inlined
    /// limit is one the planner can see.
    pub fn limit(&mut self, rows: u64) -> &mut Self {
        self.limit = Some(rows);
        self
    }

    /// Renders the rewritten query.
    ///
    /// # Errors
    ///
    /// The first failure from any fragment, or one of the rewrites this crate
    /// refuses to guess at: [`Error::SetOperation`], [`Error::Grouped`],
    /// [`Error::Orphaned`].
    pub fn sql(&self) -> Result<String, Error> {
        self.render()
    }

    /// Renders the query and hands it to sqlx with its bound values.
    ///
    /// # Errors
    ///
    /// As [`sql`](Self::sql), and [`Error::Encode`] if a value cannot be
    /// encoded for this driver.
    pub fn build(self) -> Result<SqlxQuery<'static, DB, DB::Arguments>, Error> {
        // `AssertSqlSafe` is sqlx asking who vouches for a string built at
        // runtime. This crate does: every part of it was either the query the
        // caller wrote or a fragment that parsed, and each value is bound.
        let (sql, arguments) = self.finish()?;
        Ok(sqlx::query_with(AssertSqlSafe(sql), arguments))
    }

    /// As [`build`](Self::build), mapping rows to `O`.
    ///
    /// # Errors
    ///
    /// As [`build`](Self::build).
    pub fn build_as<O>(self) -> Result<QueryAs<'static, DB, O, DB::Arguments>, Error>
    where
        O: for<'r> FromRow<'r, DB::Row>,
    {
        let (sql, arguments) = self.finish()?;
        Ok(sqlx::query_as_with(AssertSqlSafe(sql), arguments))
    }

    /// Renders the statement and checks that it has as many values as it has
    /// places to put them.
    ///
    /// Only on the way to the driver. [`sql`](Self::sql) renders without this,
    /// so a query can be inspected or logged before anything is bound.
    fn finish(self) -> Result<(String, DB::Arguments), Error> {
        let sql = self.render()?;

        // Counting is all a positional API can check. Two values of the same
        // type given the wrong way round is still a silent mistake, and only
        // naming the placeholders would catch it.
        if self.arguments.len() != self.arity {
            return Err(Error::Arity {
                wanted: self.arity,
                given: self.arguments.len(),
            });
        }

        Ok((sql, self.arguments))
    }

    /// Assembles the statement.
    fn render(&self) -> Result<String, Error> {
        if let Some(failure) = &self.failure {
            return Err(failure.clone());
        }

        let mut statement = self.statement.clone();
        let query = match &mut statement {
            Statement::Query(query) => &mut **query,
            _ => return Err(Error::NotQuery),
        };

        self.apply_filters(query)?;
        self.apply_order_by(query);
        self.apply_limit(query);

        Self::write(&statement.to_string(), self.arity)
    }

    /// Parses a fragment, marks its placeholders, and claims the slots they
    /// bind.
    ///
    /// The fragment has to be the whole of what it parsed: anything after the
    /// expression is [`Error::Trailing`]. Its own `$1` is relative to the
    /// fragment, so the slots are offset by everything claimed before it.
    fn parse<T, F>(&mut self, fragment: &str, parse: F) -> Result<T, Error>
    where
        T: Render + VisitMut,
        // `'static` rather than elided: the parser's grammar is a `&'static dyn`, so
        // the parser built from it is too, and leaving the lifetime open
        // would ask `parse_expr` to work for every parser rather than this one.
        F: FnOnce(&mut Parser<'static>) -> Result<T, ParserError>,
    {
        let mut parser = Parser::new(DB::parser())
            .try_with_sql(fragment)
            .map_err(|source| Error::Fragment {
                fragment: fragment.to_owned(),
                source,
            })?;

        let mut parsed = parse(&mut parser).map_err(|source| Error::Fragment {
            fragment: fragment.to_owned(),
            source,
        })?;

        let rest = parser.peek_token();
        if rest.token != Token::EOF {
            return Err(Error::Trailing {
                fragment: fragment.to_owned(),
                rest: rest.to_string(),
            });
        }

        self.arity += Self::number(&mut parsed, self.arity);

        Ok(parsed)
    }

    fn fail(&mut self, error: Error) {
        // First failure wins: it is the one that explains the rest.
        if self.failure.is_none() {
            self.failure = Some(error);
        }
    }

    fn apply_filters(&self, query: &mut Query) -> Result<(), Error> {
        if self.filters.is_empty() {
            return Ok(());
        }

        let SetExpr::Select(select) = &mut *query.body else {
            return Err(Error::SetOperation);
        };

        // With a GROUP BY in play there are two clauses a predicate could
        // belong to, and the fragment does not say which.
        let grouped = match &select.group_by {
            GroupByExpr::Expressions(exprs, modifiers) => {
                !exprs.is_empty() || !modifiers.is_empty()
            }
            GroupByExpr::All(_) => true,
        };
        if grouped {
            return Err(Error::Grouped);
        }

        let mut selection = select.selection.take();
        for filter in &self.filters {
            selection = Some(match selection {
                None => filter.clone(),
                Some(existing) => Expr::BinaryOp {
                    left: Box::new(parenthesize(existing)),
                    op: BinaryOperator::And,
                    right: Box::new(parenthesize(filter.clone())),
                },
            });
        }
        select.selection = selection;

        Ok(())
    }

    fn apply_order_by(&self, query: &mut Query) {
        if self.order_by.is_empty() {
            return;
        }

        let base = match query.order_by.take() {
            Some(OrderBy {
                kind: OrderByKind::Expressions(base),
                ..
            }) => base,
            _ => Vec::new(),
        };

        // First mention of a column wins, and decides both where it sits and
        // which way it sorts. Fragments come before the base query's own
        // ordering, and an earlier fragment before a later one -- so ordering
        // by a column twice is not an error, it is just the second one having
        // nothing left to say.
        //
        let mut seen: Vec<String> = Vec::new();
        let mut exprs: Vec<OrderByExpr> = Vec::new();

        for candidate in self.order_by.iter().cloned().chain(base) {
            let column = ordering_key(&candidate.expr);
            if !seen.contains(&column) {
                seen.push(column);
                exprs.push(candidate);
            }
        }

        query.order_by = Some(OrderBy {
            kind: OrderByKind::Expressions(exprs),
            interpolate: None,
        });
    }

    fn apply_limit(&self, query: &mut Query) {
        let Some(rows) = self.limit else {
            return;
        };

        query.limit_clause = Some(LimitClause::LimitOffset {
            limit: Some(Expr::Value(Value::Number(rows.to_string(), false).into())),
            offset: None,
            limit_by: Vec::new(),
        });
    }

    /// Numbers every placeholder under `node`, continuing from `base`, and
    /// returns how many values it claims.
    ///
    /// A placeholder that arrived numbered is taken at its word, so `$2` asks
    /// for the second value of this fragment and `$1` twice asks for the first
    /// one twice. A bare `?` asks for the next one, counted in the order it
    /// renders.
    fn number<V: Render + VisitMut>(node: &mut V, base: usize) -> usize {
        // Tell the nodes apart first. Which value each one wants depends on
        // where it renders, and that is not known until it has been rendered.
        let mut origins = HashMap::new();
        let mut id = 0;
        Self::substitute(node, |text| {
            origins.insert(id, text.to_owned());
            id += 1;
            format!("{PENDING}{}{SUFFIX}", id - 1)
        });

        let mut slots: HashMap<usize, usize> = HashMap::new();
        let mut counted = 0;
        let mut named = 0;

        for (_, id) in Self::scan(&node.render(), PENDING) {
            // `$2`, and SQLite's `?2`, both say which value they want. A bare
            // `?` says only that it wants one.
            let asked = origins
                .get(&id)
                .and_then(|origin| origin.strip_prefix(['$', '?']))
                .and_then(|digits| digits.parse::<usize>().ok());

            let slot = if let Some(n) = asked {
                named = named.max(n);
                n - 1
            } else {
                counted += 1;
                counted - 1
            };

            slots.insert(id, slot);
        }

        Self::substitute(node, |text| {
            let id = text
                .strip_prefix(PENDING)
                .and_then(|rest| rest.strip_suffix(SUFFIX))
                .and_then(|digits| digits.parse::<usize>().ok())
                .expect("every placeholder was just given a pending number");
            format!("{NUMBERED}{}{SUFFIX}", base + slots[&id])
        });

        named.max(counted)
    }

    /// Writes the placeholders out in the driver's own form.
    fn write(sql: &str, arity: usize) -> Result<String, Error> {
        let found = Self::scan(sql, NUMBERED);

        // Every value that was claimed has to still have somewhere to go. One
        // value in two places is fine and stays one value: both drivers here
        // name what they bind.
        let claimed: HashSet<usize> = found.iter().map(|(_, slot)| *slot).collect();
        if claimed.len() != arity {
            return Err(Error::Orphaned);
        }

        let mut out = String::with_capacity(sql.len());
        let mut at = 0;

        for (span, slot) in found {
            out.push_str(&sql[at..span.start]);
            out.push_str(&DB::placeholder(slot));
            at = span.end;
        }
        out.push_str(&sql[at..]);

        Ok(out)
    }

    /// Replaces every placeholder under `node`, handing each to `next`.
    fn substitute<V: VisitMut>(node: &mut V, mut next: impl FnMut(&str) -> String) {
        let _: ControlFlow<()> = visit_expressions_mut(node, |expr| {
            if let Expr::Value(value) = expr
                && let Value::Placeholder(text) = &mut value.value
            {
                *text = next(text);
            }
            ControlFlow::Continue(())
        });
    }

    /// Finds every placeholder written with `prefix`, in the order it renders.
    fn scan(sql: &str, prefix: &str) -> Vec<(Range<usize>, usize)> {
        let mut found = Vec::new();
        let mut at = 0;

        while let Some(offset) = sql[at..].find(prefix) {
            let start = at + offset;
            let digits = start + prefix.len();

            let Some(end) = sql[digits..].find(SUFFIX) else {
                at = digits;
                continue;
            };
            let Ok(n) = sql[digits..digits + end].parse::<usize>() else {
                at = digits;
                continue;
            };

            let stop = digits + end + SUFFIX.len();
            found.push((start..stop, n));
            at = stop;
        }

        found
    }
}

/// Shows the query being assembled, but never the values bound to it.
///
/// Written out rather than derived for two reasons: `DB::Arguments` is not
/// `Debug` for every driver, and bound values are the part of a query most
/// likely to be something that should not reach a log.
impl<DB: Syntax> fmt::Debug for QueryWriter<DB> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("QueryWriter")
            .field("statement", &self.statement.to_string())
            .field("filters", &self.filters.len())
            .field("order_by", &self.order_by.len())
            .field("limit", &self.limit)
            .field("binds", &self.arity)
            .field("failure", &self.failure)
            .finish_non_exhaustive()
    }
}

/// Wraps an expression in parentheses if joining it with `AND` would otherwise
/// change what it means.
///
/// Only `OR` needs it. `AND` binds tighter, so `a OR b` spliced beside a
/// filter becomes `a OR (b AND filter)` -- a different query, silently. Every
/// other operator that can head an expression here binds tighter than `AND`
/// already, and parenthesising those would only add noise.
fn parenthesize(expr: Expr) -> Expr {
    if matches!(
        expr,
        Expr::BinaryOp {
            op: BinaryOperator::Or,
            ..
        }
    ) {
        return Expr::Nested(Box::new(expr));
    }
    expr
}

/// A column's identity, for the purpose of ordering by it only once.
///
/// Quoting is ignored: a [`Sort`] writes `"id"` and a hand-written query
/// writes `id`, and they are the same column to the database. Anything that is
/// not a plain column -- `lower(name)`, say -- falls back to its text, which is
/// as close to an identity as an expression gets.
fn ordering_key(expr: &Expr) -> String {
    match expr {
        Expr::Identifier(ident) => ident.value.clone(),
        Expr::CompoundIdentifier(parts) => parts
            .iter()
            .map(|part| part.value.as_str())
            .collect::<Vec<_>>()
            .join("."),
        other => other.to_string(),
    }
}

/// Which way a column sorts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortDirection {
    /// `ASC`, and what a request means by naming a field with no direction.
    Asc,
    /// `DESC`.
    Desc,
}

/// One column of an ordering.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SortKey {
    /// The field a request named, or -- once [`Sort::resolve`] has run -- the
    /// column it stands for.
    pub name: String,
    /// Which way it sorts.
    pub direction: SortDirection,
}

/// An ordering, as a request asked for it.
///
/// Parsed from [AIP-132]'s `order_by`: fields separated by commas, each
/// optionally followed by `asc` or `desc`.
///
/// ```
/// # use std::collections::HashMap;
/// # use sqlx_query::Sort;
/// let columns = HashMap::from([("readCount", "read_count"), ("id", "id")]);
///
/// let sort = Sort::parse("readCount desc")?.asc("id").resolve(&columns)?;
/// # Ok::<_, sqlx_query::Error>(())
/// ```
///
/// # Parsed, then resolved
///
/// [`parse`](Self::parse) reads the syntax and nothing else, so it can run
/// wherever a request is validated, knowing about no database at all. What it
/// holds afterwards is the field names the client used.
///
/// [`resolve`](Self::resolve) turns those into columns and refuses any field
/// the map does not name. That refusal is the point: the map is an allowlist,
/// so a request can only order by what you chose to offer. A `Sort` that was
/// never resolved is refused by [`QueryBuilder::sort`] rather than written
/// into a query, so forgetting the step cannot quietly skip the allowlist.
///
/// [AIP-132]: https://google.aip.dev/132
#[derive(Debug, Clone, Default)]
pub struct Sort {
    keys: Vec<SortKey>,
    resolved: bool,
}

impl Sort {
    /// Reads an `order_by` value.
    ///
    /// Blank means no ordering was asked for rather than being an error --
    /// that is what an absent query parameter looks like by the time it
    /// arrives here.
    ///
    /// # Errors
    ///
    /// [`Error::Sort`] if a term is empty, or carries anything other than a
    /// field and an optional `asc` or `desc`.
    pub fn parse(order_by: &str) -> Result<Self, Error> {
        let mut keys = Vec::new();

        if !order_by.trim().is_empty() {
            for term in order_by.split(',') {
                keys.push(SortKey::parse(term, order_by)?);
            }
        }

        Ok(Self {
            keys,
            resolved: false,
        })
    }

    /// An ordering this program decided rather than parsed.
    ///
    /// The names are columns, not fields, so this is already resolved -- there
    /// is no client input here for an allowlist to check.
    #[must_use]
    pub fn new(keys: Vec<SortKey>) -> Self {
        Self {
            keys,
            resolved: true,
        }
    }

    /// Adds a field to order by, ascending.
    ///
    /// Appended, so it acts as a tiebreaker behind whatever the request asked
    /// for. A field the request already named stays where the request put it,
    /// in the direction the request chose: this can follow a client, never
    /// overrule one.
    ///
    /// Keyset pagination is only correct over a total ordering, which in
    /// practice means ending one with a unique column.
    #[must_use]
    pub fn asc(self, field: &str) -> Self {
        self.push(field, SortDirection::Asc)
    }

    /// Adds a field to order by, descending. As [`asc`](Self::asc) otherwise.
    #[must_use]
    pub fn desc(self, field: &str) -> Self {
        self.push(field, SortDirection::Desc)
    }

    /// Renames every field to the column it stands for.
    ///
    /// The map is the allowlist: a field it does not name is refused, so a
    /// request cannot order by a column you did not offer. A column may be
    /// qualified -- `v.created_at` is written as `"v"."created_at"`.
    ///
    /// Calling this twice does nothing the second time; the names are already
    /// columns by then.
    ///
    /// # Errors
    ///
    /// [`Error::Field`], naming the first field the map does not have.
    pub fn resolve(mut self, columns: &HashMap<&str, &str>) -> Result<Self, Error> {
        if self.resolved {
            return Ok(self);
        }

        for key in &mut self.keys {
            let column = columns
                .get(key.name.as_str())
                .ok_or_else(|| Error::Field(key.name.clone()))?;
            key.name = (*column).to_owned();
        }

        self.resolved = true;
        Ok(self)
    }

    /// The columns being ordered by, in order.
    #[must_use]
    pub fn keys(&self) -> &[SortKey] {
        &self.keys
    }

    /// Whether nothing is being ordered by.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    fn push(mut self, field: &str, direction: SortDirection) -> Self {
        if !self.keys.iter().any(|key| key.name == field) {
            self.keys.push(SortKey {
                name: field.to_owned(),
                direction,
            });
        }
        self
    }

    /// The ordering as syntax, ready to graft onto a query.
    fn order_by(&self) -> Vec<OrderByExpr> {
        self.keys
            .iter()
            .map(|key| OrderByExpr {
                expr: quoted(&key.name),
                // Written out even for ascending, which is already the
                // default: the request said which way round it wanted, and a
                // query that says so too is easier to read back.
                options: OrderByOptions {
                    sort: Some(match key.direction {
                        SortDirection::Asc => OrderBySort::Asc,
                        SortDirection::Desc => OrderBySort::Desc,
                    }),
                    nulls_first: None,
                },
                with_fill: None,
            })
            .collect()
    }
}

impl SortKey {
    /// `order_by` is carried along only so an error can quote what was
    /// actually sent, rather than one term out of context.
    fn parse(term: &str, order_by: &str) -> Result<Self, Error> {
        let mut words = term.split_whitespace();

        let Some(name) = words.next() else {
            return Err(Error::Sort(format!(
                "`{order_by}` has an empty term; each one is a field, optionally \
                 followed by `asc` or `desc`"
            )));
        };

        let direction = match words.next() {
            None => SortDirection::Asc,
            Some(word) if word.eq_ignore_ascii_case("asc") => SortDirection::Asc,
            Some(word) if word.eq_ignore_ascii_case("desc") => SortDirection::Desc,
            Some(word) => {
                return Err(Error::Sort(format!(
                    "`{order_by}` says `{word}` after `{name}`, which is neither `asc` nor `desc`"
                )));
            }
        };

        if let Some(extra) = words.next() {
            return Err(Error::Sort(format!(
                "`{order_by}` has `{extra}` after `{name}`, which is one word too many"
            )));
        }

        Ok(Self {
            name: name.to_owned(),
            direction,
        })
    }
}

/// Writes a column name as syntax, quoted so a column called `order` is still
/// a column.
///
/// A dotted name is a qualified one: `v.created_at` is `"v"."created_at"`,
/// rather than one column with a dot in its name.
fn quoted(name: &str) -> Expr {
    let mut parts: Vec<Ident> = name
        .split('.')
        .map(|part| Ident::with_quote('"', part))
        .collect();

    if parts.len() == 1 {
        Expr::Identifier(parts.remove(0))
    } else {
        Expr::CompoundIdentifier(parts)
    }
}

/// Adds what a request asked for to a query you already wrote.
///
/// The same rewrite [`QueryWriter`] performs, but taking what a client sent --
/// already parsed and checked against an allowlist -- instead of SQL you wrote
/// yourself. That is the whole difference between the two:
///
/// | | takes |
/// | --- | --- |
/// | [`QueryWriter`] | SQL fragments, which you vouch for |
/// | `QueryBuilder` | [`Sort`] and the like, which went through an allowlist |
///
/// Keeping them apart matters because an AIP `order_by` and a SQL `ORDER BY`
/// fragment look identical -- `"title desc"` is both -- so a single type
/// offering both would let a client's string reach the unchecked path without
/// anything looking wrong.
///
/// ```
/// # #[cfg(feature = "postgres")] {
/// # use std::collections::HashMap;
/// use sqlx::Postgres;
/// use sqlx_query::{QueryBuilder, Sort};
///
/// let columns = HashMap::from([("title", "title"), ("id", "id")]);
/// let sort = Sort::parse("title desc")?.asc("id").resolve(&columns)?;
///
/// let mut query = QueryBuilder::<Postgres>::new(
///     "SELECT id, title FROM volumes WHERE tenant_id = $1",
/// )?;
/// query.bind(7_i64).sort(&sort).limit(50);
///
/// assert_eq!(
///     query.sql()?,
///     "SELECT id, title FROM volumes WHERE tenant_id = $1 \
///      ORDER BY \"title\" DESC, \"id\" ASC LIMIT 50",
/// );
/// # }
/// # Ok::<_, sqlx_query::Error>(())
/// ```
///
/// For a fragment you wrote yourself, reach for [`QueryWriter`] instead.
pub struct QueryBuilder<DB: Syntax> {
    writer: QueryWriter<DB>,
}

impl<DB: Syntax> QueryBuilder<DB> {
    /// Parses the query to add to.
    ///
    /// # Errors
    ///
    /// As [`QueryWriter::new`].
    pub fn new(sql: &str) -> Result<Self, Error> {
        Ok(Self {
            writer: QueryWriter::new(sql)?,
        })
    }

    /// Binds the next value, as [`QueryWriter::bind`].
    pub fn bind<'t, T>(&mut self, value: T) -> &mut Self
    where
        T: Encode<'t, DB> + Type<DB>,
    {
        self.writer.bind(value);
        self
    }

    /// Orders by what the request asked for.
    ///
    /// Whatever the query already ordered by drops behind it as a tiebreaker,
    /// and a column named by both is ordered by once, where the request put
    /// it.
    ///
    /// A [`Sort`] that was never resolved is refused -- see
    /// [`Error::Unresolved`] -- because its names are still the client's
    /// fields, and writing those into a query is exactly what the allowlist
    /// exists to prevent.
    pub fn sort(&mut self, sort: &Sort) -> &mut Self {
        if sort.resolved {
            self.writer.order_by.extend(sort.order_by());
        } else {
            self.writer.fail(Error::Unresolved);
        }
        self
    }

    /// Sets `LIMIT`, as [`QueryWriter::limit`].
    pub fn limit(&mut self, rows: u64) -> &mut Self {
        self.writer.limit(rows);
        self
    }

    /// Renders the query.
    ///
    /// # Errors
    ///
    /// As [`QueryWriter::sql`], and [`Error::Unresolved`].
    pub fn sql(&self) -> Result<String, Error> {
        self.writer.sql()
    }

    /// Renders the query and hands it to sqlx, as [`QueryWriter::build`].
    ///
    /// # Errors
    ///
    /// As [`sql`](Self::sql).
    pub fn build(self) -> Result<SqlxQuery<'static, DB, DB::Arguments>, Error> {
        self.writer.build()
    }

    /// As [`build`](Self::build), mapping rows to `O`.
    ///
    /// # Errors
    ///
    /// As [`sql`](Self::sql).
    pub fn build_as<O>(self) -> Result<QueryAs<'static, DB, O, DB::Arguments>, Error>
    where
        O: for<'r> FromRow<'r, DB::Row>,
    {
        self.writer.build_as()
    }

    /// The query underneath, for anything this layer does not cover.
    ///
    /// Its methods take SQL fragments rather than request objects, so whatever
    /// goes in through here is yours to vouch for.
    pub fn writer(&mut self) -> &mut QueryWriter<DB> {
        &mut self.writer
    }
}

impl<DB: Syntax> fmt::Debug for QueryBuilder<DB> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("QueryBuilder")
            .field("writer", &self.writer)
            .finish()
    }
}

/// What can go wrong between a query you wrote and the one that runs.
///
/// Every variant is raised before the database is touched: a rewrite either
/// produces a statement this crate is willing to vouch for, or it produces
/// this.
// `Clone` so that a rewrite which failed while the chain was still being built
// can report the same failure from every later `sql()` or `build()`, rather
// than reporting it once and then appearing to succeed.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum Error {
    /// The base query did not parse.
    Query(ParserError),

    /// A fragment did not parse as SQL.
    ///
    /// The fragment is carried along because the caller usually did not write
    /// it by hand -- it arrived from a filter compiler, and the text is the
    /// only way to see what that compiler emitted.
    Fragment {
        /// The fragment as given.
        fragment: String,
        /// Why the parser rejected it.
        source: ParserError,
    },

    /// A fragment parsed, but only a prefix of it was an expression.
    ///
    /// This is the variant that makes fragments safe to accept as text.
    /// `role = 'admin'` parses and consumes everything; `role = 'admin';
    /// DROP TABLE users` parses an expression and leaves a statement behind,
    /// and that leftover is refused here rather than spliced.
    Trailing {
        /// The fragment as given.
        fragment: String,
        /// The first token that was not part of the expression.
        rest: String,
    },

    /// The base SQL was not a query, so there is no `WHERE` to add to.
    NotQuery,

    /// The base query's outermost level is a `UNION`, `INTERSECT` or `EXCEPT`.
    ///
    /// There is no single `SELECT` to attach a filter to, and picking one of
    /// the branches would silently filter half the result. Wrap the set
    /// operation in an outer `SELECT ... FROM (...) AS t` and rewrite that.
    SetOperation,

    /// The base query has a `GROUP BY`, so a filter is ambiguous.
    ///
    /// A predicate over a grouping column belongs in `WHERE`, one over an
    /// aggregate belongs in `HAVING`, and the two run at different times
    /// against different rows. Nothing in the fragment says which was meant.
    Grouped,

    /// A bound value could not be encoded for this driver.
    ///
    /// Carried as text because sqlx's own encode error is not `Clone`, and
    /// this one has to survive being reported from more than one call.
    Encode(String),

    /// An `order_by` value did not parse.
    Sort(String),

    /// A request named a field the column map does not have.
    ///
    /// The map is an allowlist, so this is what stops a request ordering by a
    /// column you did not offer.
    Field(String),

    /// A [`Sort`] reached the query without being resolved.
    ///
    /// Its names are still the client's field names, which is exactly what
    /// [`Sort::resolve`] exists to turn into columns -- and to refuse.
    Unresolved,

    /// The statement wants a different number of values than were bound.
    ///
    /// Placeholders are claimed as they are parsed -- the base query's first,
    /// then each fragment's -- and the values are given in that same order, so
    /// a mismatch usually means a fragment's were forgotten or given twice.
    ///
    /// Counting is as far as this goes. Two values of the same type supplied
    /// the wrong way round is still a silent mistake, and only naming the
    /// placeholders rather than numbering them would catch it.
    Arity {
        /// How many placeholders the statement has.
        wanted: usize,
        /// How many values were bound.
        given: usize,
    },

    /// Some value the statement asks for has no placeholder left to bind to.
    ///
    /// Two ways to arrive here. [`limit`](QueryWriter::limit) replaces the
    /// query's own `LIMIT`, so a base query that said `LIMIT $2` loses `$2`
    /// and the value bound for it has nowhere to go -- take the `LIMIT` out of
    /// the base query, or keep it and do not call `limit`.
    ///
    /// Or the base query skipped a number: `WHERE a = $2` with no `$1` claims
    /// two values and uses one. PostgreSQL refuses that too, for the same
    /// reason.
    Orphaned,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Query(source) => write!(f, "the query did not parse: {source}"),
            Self::Fragment { fragment, source } => {
                write!(f, "the fragment `{fragment}` did not parse: {source}")
            }
            Self::Trailing { fragment, rest } => write!(
                f,
                "the fragment `{fragment}` is an expression followed by `{rest}`; \
                 a fragment has to be one complete expression and nothing else",
            ),
            Self::NotQuery => f.write_str("the SQL is not a query, so it has no WHERE to add to"),
            Self::SetOperation => f.write_str(
                "the query's outermost level is a set operation, which has no single SELECT \
                 to filter; wrap it in `SELECT * FROM (...) AS t` and rewrite that instead",
            ),
            Self::Encode(message) => write!(f, "a bound value could not be encoded: {message}"),
            Self::Sort(message) => write!(f, "the ordering did not parse: {message}"),
            Self::Field(field) => write!(
                f,
                "`{field}` is not a field this query offers; only the ones named in its \
                 column map can be ordered or filtered by",
            ),
            Self::Unresolved => f.write_str(
                "this ordering still holds the field names a request sent; call \
                 `Sort::resolve` so they are checked against the column map and turned \
                 into columns",
            ),
            Self::Arity { wanted, given } => write!(
                f,
                "the statement has {wanted} placeholders but {given} values were bound; \
                 values are given in the order the placeholders claim them, the base \
                 query's first and then each fragment's",
            ),
            Self::Orphaned => f.write_str(
                "a value this statement asks for has no placeholder to bind to: either the \
                 base query skips a number, as `WHERE a = $2` does with no `$1`, or `limit()` \
                 replaced a `LIMIT` that held one",
            ),
            Self::Grouped => f.write_str(
                "the query has a GROUP BY, so a filter could mean WHERE or HAVING; \
                 put the predicate in the query itself",
            ),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Query(source) | Self::Fragment { source, .. } => Some(source),
            _ => None,
        }
    }
}
