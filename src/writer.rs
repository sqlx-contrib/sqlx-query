use std::collections::HashMap;
use std::fmt;

use sqlparser::ast::{
    BinaryOperator, Expr, GroupByExpr, LimitClause, OrderBy, OrderByExpr, OrderByKind, Query,
    SetExpr, Statement, Value,
};
use sqlparser::parser::Parser;
use sqlparser::tokenizer::Token;
use sqlx::error::BoxDynError;
use sqlx::query::{Query as SqlxQuery, QueryAs};
use sqlx::{Arguments, AssertSqlSafe, Encode, FromRow, Type};

use crate::dialect::Dialect;
use crate::{Error, placeholder};

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
/// a request. It is parsed with the same dialect as the query, and it has to
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
pub struct QueryWriter<'a, DB: Dialect> {
    statement: Statement,
    filters: Vec<Expr>,
    order_by: Vec<OrderByExpr>,
    limit: Option<u64>,

    // Marker bookkeeping. `slots` maps every marker installed so far to the
    // value it binds; `arity` is how many values have been claimed, and so
    // where the next fragment's own numbering starts from.
    origins: HashMap<usize, String>,
    slots: HashMap<usize, usize>,
    next_marker: usize,
    arity: usize,

    binds: Vec<Bind<'a, DB>>,
    failure: Option<Error>,
}

impl<'a, DB: Dialect> QueryWriter<'a, DB> {
    /// Parses the query to rewrite.
    ///
    /// # Errors
    ///
    /// [`Error::Query`] if the SQL does not parse in this driver's dialect,
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

        let mut origins = HashMap::new();
        let mut next_marker = 0;
        placeholder::mark(&mut statement, &mut next_marker, &mut origins);

        // The base query's own numbering, read back from how it renders. Its
        // placeholders are bound first, so its slots are the global ones.
        let base = placeholder::region(&statement.to_string(), &origins);

        Ok(Self {
            statement,
            filters: Vec::new(),
            order_by: Vec::new(),
            limit: None,
            origins,
            slots: base.slots,
            next_marker,
            arity: base.arity,
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

        placeholder::write::<DB>(&statement.to_string(), &self.slots, self.arity)
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
        T: Rendered + sqlparser::ast::VisitMut,
        // `'static` rather than elided: the dialect is a `&'static dyn`, so
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

        let mut origins = HashMap::new();
        placeholder::mark(&mut parsed, &mut self.next_marker, &mut origins);

        let region = placeholder::region(&parsed.rendered(), &origins);
        for (id, slot) in region.slots {
            self.slots.insert(id, self.arity + slot);
        }
        self.origins.extend(origins);
        self.arity += region.arity;

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

        let mut exprs = self.order_by.clone();

        if let Some(existing) = query.order_by.take()
            && let OrderByKind::Expressions(base) = existing.kind
        {
            // Compared as rendered text: `name` and `"name"` are different
            // orderings to the database too, so matching them here would be
            // the wrong kind of clever.
            let kept: Vec<String> = exprs.iter().map(|e| e.expr.to_string()).collect();
            exprs.extend(
                base.into_iter()
                    .filter(|candidate| !kept.contains(&candidate.expr.to_string())),
            );
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
}

/// Shows the query being assembled, but never the values bound to it.
///
/// Written out rather than derived for two reasons: `DB::Arguments` is not
/// `Debug` for every driver, and bound values are the part of a query most
/// likely to be something that should not reach a log.
impl<DB: Dialect> fmt::Debug for QueryWriter<'_, DB> {
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

/// Renders a freshly parsed fragment, so its placeholders can be read back in
/// the order they will print.
///
/// `Expr` has a `Display`; a list of `OrderByExpr` does not, because sqlparser
/// only ever prints one inside a clause.
trait Rendered {
    fn rendered(&self) -> String;
}

impl Rendered for Expr {
    fn rendered(&self) -> String {
        self.to_string()
    }
}

impl Rendered for Vec<OrderByExpr> {
    fn rendered(&self) -> String {
        self.iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(", ")
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
