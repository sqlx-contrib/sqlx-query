use std::collections::{HashMap, HashSet};
use std::fmt;
use std::ops::{ControlFlow, Range};

use sqlparser::ast::{
    BinaryOperator, Expr, GroupByExpr, LimitClause, OrderBy, OrderByExpr, OrderByKind, Query,
    SetExpr, Statement, Value, VisitMut, visit_expressions_mut,
};
use sqlparser::parser::Parser;
use sqlparser::tokenizer::Token;
use sqlx::error::BoxDynError;
use sqlx::query::{Query as SqlxQuery, QueryAs};
use sqlx::{Arguments, AssertSqlSafe, Encode, FromRow, Type};

use crate::Error;
use crate::syntax::Syntax;

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

/// One value, kept until the statement is rendered.
///
/// The values cannot go straight into `DB::Arguments`, because that is
/// append-only and the order they are bound in is not known until the SQL has
/// been laid out -- a fragment spliced into the middle of a `?` query shifts
/// everything after it. Holding each one as a closure lets them be replayed in
/// whatever order the finished statement asks for.
type Bind<'a, DB> =
    Box<dyn FnOnce(&mut <DB as sqlx::Database>::Arguments) -> Result<(), BoxDynError> + Send + 'a>;

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
/// writer.bind(7_i64).filter_by("role = 'admin'").order_by("name asc");
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
pub struct QueryWriter<'a, DB: Syntax> {
    statement: Statement,
    filters: Vec<Expr>,
    order_by: Vec<OrderByExpr>,
    limit: Option<u64>,

    // How many values have been claimed so far, and so where the next
    // fragment's own numbering starts from.
    arity: usize,

    binds: Vec<Bind<'a, DB>>,
    failure: Option<Error>,
}

impl<'a, DB: Syntax> QueryWriter<'a, DB> {
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
            binds: Vec::new(),
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
    /// That is the order they are *given* in, not necessarily the order they
    /// are sent in. A fragment spliced into the middle of the query renders
    /// its placeholder there, and for a driver that binds `?` by position the
    /// values are replayed to match. Nothing about that is visible here.
    pub fn bind<T>(&mut self, value: T) -> &mut Self
    where
        T: Encode<'a, DB> + Type<DB> + Send + 'a,
    {
        self.binds.push(Box::new(move |args| args.add(value)));
        self
    }

    /// Joins a fragment onto the query's `WHERE` with `AND`.
    ///
    /// Called more than once, the fragments are `AND`ed together. An existing
    /// `WHERE` is kept and joined the same way -- this adds a condition, it
    /// never replaces one.
    pub fn filter_by(&mut self, fragment: &str) -> &mut Self {
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
        Ok(self.render()?.0)
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

    /// Assembles the statement and reports which value each placeholder takes,
    /// in the order they render.
    fn render(&self) -> Result<(String, Vec<usize>), Error> {
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

    /// Renders the statement and replays the values into it.
    ///
    /// A driver that numbers its placeholders takes the values in the order
    /// they were given, since each placeholder says which one it wants. One
    /// that binds by position takes them in the order they render, which is
    /// the only thing that says the same.
    fn finish(mut self) -> Result<(String, DB::Arguments), Error> {
        let (sql, order) = self.render()?;

        let mut arguments = DB::Arguments::default();
        let sequence: Vec<usize> = if DB::positional() {
            order
        } else {
            (0..self.arity).collect()
        };

        // Draining by slot rather than in order, because the closures are
        // `FnOnce` and the sequence is a permutation rather than a walk.
        let mut binds: Vec<Option<Bind<'a, DB>>> = self.binds.drain(..).map(Some).collect();
        for slot in sequence {
            let Some(bind) = binds.get_mut(slot).and_then(Option::take) else {
                return Err(Error::Unbound {
                    wanted: self.arity,
                    given: binds.len(),
                });
            };
            bind(&mut arguments).map_err(|error| Error::Encode(error.to_string()))?;
        }

        Ok((sql, arguments))
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
        F: FnOnce(&mut Parser<'static>) -> Result<T, sqlparser::parser::ParserError>,
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
        // Compared as rendered text: `name` and `"name"` are different
        // orderings to the database too, so matching them here would be the
        // wrong kind of clever.
        let mut seen: Vec<String> = Vec::new();
        let mut exprs: Vec<OrderByExpr> = Vec::new();

        for candidate in self.order_by.iter().cloned().chain(base) {
            let column = candidate.expr.to_string();
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

    /// Writes the placeholders out in the driver's own form, and says which
    /// value each one takes.
    ///
    /// The returned numbers are in render order. For a driver whose
    /// placeholder carries no number that is the order it binds in, so this is
    /// also what says how the values have to be sent.
    fn write(sql: &str, arity: usize) -> Result<(String, Vec<usize>), Error> {
        let found = Self::scan(sql, NUMBERED);
        let order: Vec<usize> = found.iter().map(|(_, slot)| *slot).collect();

        let distinct: HashSet<usize> = order.iter().copied().collect();
        if distinct.len() != arity {
            return Err(Error::Orphaned);
        }

        // A placeholder with no number of its own takes a value per appearance
        // and cannot ask for an earlier one, so one value in two places cannot
        // be written at all.
        if DB::positional() && order.len() != distinct.len() {
            return Err(Error::Positional);
        }

        let mut out = String::with_capacity(sql.len());
        let mut at = 0;

        for (span, slot) in found {
            out.push_str(&sql[at..span.start]);
            out.push_str(&DB::placeholder(slot));
            at = span.end;
        }
        out.push_str(&sql[at..]);

        Ok((out, order))
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
impl<DB: Syntax> fmt::Debug for QueryWriter<'_, DB> {
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
