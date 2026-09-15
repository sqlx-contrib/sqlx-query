//! What a driver can be asked for, and what a request can ask.
//!
//! Everything here is either a driver's own vocabulary -- how it spells a
//! placeholder, how it binds a value -- or a request's, parsed and checked
//! before it is allowed anywhere near a query. [`QueryWriter`](crate::QueryWriter)
//! is what puts the two together.

use std::collections::HashMap;

use sqlparser::ast::{Expr, Ident, OrderByExpr, OrderByOptions, OrderBySort};
use sqlparser::dialect::Dialect;
use sqlparser::parser::{Parser, ParserError};
use sqlx::error::BoxDynError;

use std::fmt;

use crate::writer::QueryWriter;

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

    /// Binds a filter's literal, in this driver's own types.
    ///
    /// Here rather than on [`QueryWriter`] because a writer is generic over
    /// the driver, and generic code cannot know that `i64` is bindable for it.
    /// An implementation knows, because it names one driver.
    ///
    /// # Errors
    ///
    /// Whatever the driver says when a value cannot be encoded.
    fn bind_literal(arguments: &mut Self::Arguments, literal: Literal) -> Result<(), BoxDynError>;

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
    use super::{BoxDynError, Dialect, Literal, Syntax};
    use sqlparser::dialect::PostgreSqlDialect;
    use sqlx::Arguments as _;

    static DIALECT: PostgreSqlDialect = PostgreSqlDialect {};

    impl Syntax for sqlx::Postgres {
        fn parser() -> &'static dyn Dialect {
            &DIALECT
        }

        fn bind_literal(
            arguments: &mut Self::Arguments,
            literal: Literal,
        ) -> Result<(), BoxDynError> {
            match literal {
                Literal::Bool(value) => arguments.add(value),
                Literal::Int(value) => arguments.add(value),
                Literal::Float(value) => arguments.add(value),
                Literal::Text(value) => arguments.add(value),
                #[cfg(feature = "chrono")]
                Literal::Timestamp(value) => arguments.add(value),
            }
        }

        fn placeholder(index: usize) -> String {
            format!("${}", index + 1)
        }
    }
}

#[cfg(feature = "sqlite")]
mod sqlite {
    use super::{BoxDynError, Dialect, Literal, Syntax};
    use sqlparser::dialect::SQLiteDialect;
    use sqlx::Arguments as _;

    static DIALECT: SQLiteDialect = SQLiteDialect {};

    impl Syntax for sqlx::Sqlite {
        fn parser() -> &'static dyn Dialect {
            &DIALECT
        }

        fn bind_literal(
            arguments: &mut Self::Arguments,
            literal: Literal,
        ) -> Result<(), BoxDynError> {
            match literal {
                Literal::Bool(value) => arguments.add(value),
                Literal::Int(value) => arguments.add(value),
                Literal::Float(value) => arguments.add(value),
                Literal::Text(value) => arguments.add(value),
                #[cfg(feature = "chrono")]
                Literal::Timestamp(value) => arguments.add(value),
            }
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
/// never resolved is refused by [`QueryWriter::sort`] rather than written
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
pub(crate) fn quoted(name: &str) -> Expr {
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

/// A value a filter wants bound.
///
/// Closed, because it is only ever what a filter expression can hold. Values
/// travel beside the SQL rather than inside it: writing `100` into the
/// statement would be safe -- sqlparser escapes what it prints -- but it makes
/// a distinct query text per value, and a prepared statement cache keyed on
/// that text has nothing to reuse.
#[derive(Debug, Clone)]
pub enum Literal {
    Bool(bool),
    Int(i64),
    Float(f64),
    Text(String),
    /// What `timestamp('...')` in a filter folds into.
    #[cfg(feature = "chrono")]
    #[cfg_attr(docsrs, doc(cfg(feature = "chrono")))]
    Timestamp(chrono::DateTime<chrono::Utc>),
}

/// A condition, ready to join onto a query's `WHERE`, and the values it binds.
///
/// Opaque, and produced by [`IntoFilterExpr`] rather than constructed: it is
/// either a fragment that parsed, or something that came through an allowlist.
pub struct FilterExpr {
    /// `None` when nothing was asked for, so that an absent filter adds no
    /// condition rather than a `TRUE` for the planner to discard.
    pub(crate) expr: Option<Expr>,
    /// In the order the condition's placeholders render, so they are claimed
    /// and bound in step.
    pub(crate) values: Vec<Literal>,
}

/// An ordering, ready to write into a query.
///
/// Opaque, and produced by [`IntoSortExpr`]. It holds syntax rather than field
/// names, which is why a fragment may order by an expression -- `lower(name)
/// desc` -- while a [`Sort`] may only name columns.
pub struct SortExpr(Vec<OrderByExpr>);

impl SortExpr {
    /// The ordering as syntax, for the writer to graft on.
    pub(crate) fn into_inner(self) -> Vec<OrderByExpr> {
        self.0
    }
}

/// Anything [`QueryWriter::filter`] will take.
///
/// A `&str` is a SQL fragment: it is parsed, it has to be one complete
/// expression, and nothing checks what it names -- you wrote it, so you vouch
/// for it. Anything that went through an allowlist implements this too, and
/// the call site shows which you passed.
pub trait IntoFilterExpr {
    /// Turns this into a condition, parsing it in `DB`'s syntax if it is text.
    ///
    /// # Errors
    ///
    /// [`Error::Fragment`] or [`Error::Trailing`] if a fragment does not parse
    /// as exactly one expression.
    fn into_filter_expr<DB: Syntax>(self) -> Result<FilterExpr, Error>;
}

impl<S: AsRef<str>> IntoFilterExpr for S {
    fn into_filter_expr<DB: Syntax>(self) -> Result<FilterExpr, Error> {
        // A fragment binds nothing of its own: its placeholders are the
        // caller's, and so are the values that fill them.
        QueryWriter::<DB>::fragment(self.as_ref(), Parser::parse_expr).map(|expr| FilterExpr {
            expr: Some(expr),
            values: Vec::new(),
        })
    }
}

/// Anything [`QueryWriter::sort`] will take.
///
/// A `&str` is a SQL fragment, parsed as an `ORDER BY` list, so it may order
/// by an expression as well as a column. A [`&Sort`](Sort) is what a request
/// asked for, and has to have been resolved against a column map first --
/// otherwise it still holds the client's field names, and writing those into a
/// query is what the allowlist exists to prevent.
pub trait IntoSortExpr {
    /// Turns this into an ordering, parsing it in `DB`'s syntax if it is text.
    ///
    /// # Errors
    ///
    /// [`Error::Fragment`] or [`Error::Trailing`] if a fragment does not parse,
    /// and [`Error::Unresolved`] if a [`Sort`] was never resolved.
    fn into_sort_expr<DB: Syntax>(self) -> Result<SortExpr, Error>;
}

impl<S: AsRef<str>> IntoSortExpr for S {
    fn into_sort_expr<DB: Syntax>(self) -> Result<SortExpr, Error> {
        QueryWriter::<DB>::fragment(self.as_ref(), |parser| {
            parser.parse_comma_separated(Parser::parse_order_by_expr)
        })
        .map(SortExpr)
    }
}

impl IntoSortExpr for &Sort {
    fn into_sort_expr<DB: Syntax>(self) -> Result<SortExpr, Error> {
        if self.resolved {
            Ok(SortExpr(self.order_by()))
        } else {
            Err(Error::Unresolved)
        }
    }
}

#[cfg(feature = "cel")]
mod cel_filter {
    use std::collections::HashMap;

    use cel::common::ast::{Expr as CelExpr, IdedExpr, LiteralValue, operators};
    use cel::parser::Parser as CelParser;
    use sqlparser::ast::{BinaryOperator, Expr, Value as SqlValue};

    use super::{FilterExpr, IntoFilterExpr, Literal, Syntax, quoted};
    use crate::Error;
    use crate::writer::parenthesize;

    /// A condition, as a request asked for it.
    ///
    /// Parsed from [AIP-160]'s `filter`, which is [CEL]: comparisons joined with
    /// `&&`, `||` and `!`, over the fields a query chooses to offer.
    ///
    /// ```
    /// # #[cfg(feature = "cel")] {
    /// # use std::collections::HashMap;
    /// # use sqlx_query::Filter;
    /// let columns = HashMap::from([("readCount", "read_count"), ("title", "title")]);
    ///
    /// let filter = Filter::parse(r#"readCount > 100 && title.startsWith("D")"#)?
    ///     .resolve(&columns)?;
    /// # }
    /// # Ok::<_, sqlx_query::Error>(())
    /// ```
    ///
    /// # Parsed, then resolved
    ///
    /// As [`Sort`](crate::Sort), and for the same reason. [`parse`](Self::parse)
    /// reads CEL and nothing else, so it can run wherever a request is validated.
    /// [`resolve`](Self::resolve) turns the client's field names into columns and
    /// refuses any the map does not name.
    ///
    /// A `Filter` that was never resolved is refused by
    /// [`QueryWriter::filter`](crate::QueryWriter::filter) rather than written
    /// into a query. That matters more here than it does for an ordering: an
    /// unchecked filter is a client choosing which rows it may see.
    ///
    /// # Values are bound, never written in
    ///
    /// `readCount > 100` becomes `"read_count" > $2` with `100` bound. Nothing a
    /// request sends is ever rendered into the SQL, so there is no escaping to get
    /// right and no query text that varies per value.
    ///
    /// [AIP-160]: https://google.aip.dev/160
    /// [CEL]: https://github.com/google/cel-spec
    #[derive(Debug, Clone)]
    pub struct Filter {
        /// `None` when nothing was asked for, which is not the same as a condition
        /// that happens to match everything.
        condition: Option<Condition>,
        resolved: bool,
    }

    /// What a filter says, over field names until [`Filter::resolve`] makes them
    /// columns.
    #[derive(Debug, Clone)]
    enum Condition {
        /// A bare field, which is a column that is already a boolean.
        Truthy(String),
        Compare {
            field: String,
            operator: BinaryOperator,
            value: Literal,
        },
        /// `field == null`, which is `IS NULL` rather than a comparison -- nothing
        /// equals null in SQL, including null.
        IsNull {
            field: String,
            negated: bool,
        },
        In {
            field: String,
            values: Vec<Literal>,
        },
        /// `startsWith`, `endsWith` and `contains`, all of which are `LIKE` with
        /// the pattern built and bound.
        Like {
            field: String,
            pattern: String,
        },
        Not(Box<Condition>),
        All(Vec<Condition>),
        Any(Vec<Condition>),
    }

    /// The escape character for a `LIKE` pattern.
    ///
    /// A backslash would be the obvious choice and is the wrong one: MySQL treats
    /// it as an escape inside string literals as well, and SQLite does not, so the
    /// same pattern means two things. `!` is punctuation in every dialect here.
    const ESCAPE: char = '!';

    impl Filter {
        /// Reads a `filter` value.
        ///
        /// Blank means nothing was asked for rather than being an error -- that is
        /// what an absent query parameter looks like by the time it arrives here.
        ///
        /// # Errors
        ///
        /// [`Error::Filter`] if the CEL does not parse, or if it parses into
        /// something with no meaning as a SQL condition -- arithmetic, a macro,
        /// two literals compared to each other.
        pub fn parse(filter: &str) -> Result<Self, Error> {
            if filter.trim().is_empty() {
                return Ok(Self {
                    condition: None,
                    resolved: false,
                });
            }

            let parsed = CelParser::new().parse(filter).map_err(|errors| {
                Error::Filter(format!("`{filter}` is not a CEL expression: {errors}"))
            })?;

            Ok(Self {
                condition: Some(Condition::lower(&parsed)?),
                resolved: false,
            })
        }

        /// Renames every field to the column it stands for.
        ///
        /// The map is the allowlist, and the same one [`Sort`](crate::Sort) uses:
        /// a field it does not name is refused, so a request cannot filter on a
        /// column you did not offer.
        ///
        /// Calling this twice does nothing the second time.
        ///
        /// # Errors
        ///
        /// [`Error::Field`], naming the first field the map does not have.
        pub fn resolve(mut self, columns: &HashMap<&str, &str>) -> Result<Self, Error> {
            if self.resolved {
                return Ok(self);
            }

            if let Some(condition) = &mut self.condition {
                condition.resolve(columns)?;
            }

            self.resolved = true;
            Ok(self)
        }

        /// Whether nothing is being filtered on.
        #[must_use]
        pub fn is_empty(&self) -> bool {
            self.condition.is_none()
        }

        pub(crate) fn is_resolved(&self) -> bool {
            self.resolved
        }

        /// The condition as syntax, and the values it binds, in the order its
        /// placeholders will render.
        pub(crate) fn filter_expr(&self) -> Option<FilterExpr> {
            let condition = self.condition.as_ref()?;

            let mut values = Vec::new();
            let expr = condition.write(&mut values);

            Some(FilterExpr {
                expr: Some(expr),
                values,
            })
        }
    }

    impl IntoFilterExpr for &Filter {
        fn into_filter_expr<DB: Syntax>(self) -> Result<FilterExpr, Error> {
            if !self.is_resolved() {
                return Err(Error::Unresolved);
            }

            // Nothing asked for adds no condition at all, rather than a `TRUE`
            // for the planner to discard and a reader to wonder about.
            Ok(self.filter_expr().unwrap_or_else(|| FilterExpr {
                expr: None,
                values: Vec::new(),
            }))
        }
    }

    impl Condition {
        /// Turns one CEL node into a condition, or says why it cannot.
        fn lower(node: &IdedExpr) -> Result<Self, Error> {
            match &node.expr {
                CelExpr::Ident(field) => Ok(Self::Truthy(field.clone())),
                CelExpr::Call(call) => Self::lower_call(node, call),
                other => Err(Error::Filter(format!(
                    "{} is not a condition; a filter is comparisons joined with \
                     `&&`, `||` and `!`",
                    describe(other)
                ))),
            }
        }

        fn lower_call(node: &IdedExpr, call: &cel::common::ast::CallExpr) -> Result<Self, Error> {
            let name = call.func_name.as_str();

            // The string methods are the only calls with a target, and the target
            // is the field being matched.
            if let Some(target) = &call.target {
                return Self::lower_method(name, target, &call.args);
            }

            match name {
                operators::LOGICAL_AND => Ok(Self::All(Self::lower_all(&call.args)?)),
                operators::LOGICAL_OR => Ok(Self::Any(Self::lower_all(&call.args)?)),
                operators::LOGICAL_NOT => Ok(Self::Not(Box::new(Self::lower(&call.args[0])?))),
                operators::IN => Self::lower_in(&call.args),
                operators::EQUALS
                | operators::NOT_EQUALS
                | operators::LESS
                | operators::LESS_EQUALS
                | operators::GREATER
                | operators::GREATER_EQUALS => Self::lower_compare(name, &call.args),
                _ => Err(Error::Filter(format!(
                    "`{}` is not something this filter language does",
                    readable(name, node)
                ))),
            }
        }

        fn lower_all(args: &[IdedExpr]) -> Result<Vec<Self>, Error> {
            args.iter().map(Self::lower).collect()
        }

        fn lower_compare(name: &str, args: &[IdedExpr]) -> Result<Self, Error> {
            let (field, value) = pair(args, name)?;

            // `x == null` is `IS NULL`. Comparing to null with `=` is always
            // unknown in SQL, so the obvious translation would silently match
            // nothing.
            if matches!(value, Compared::Null) {
                return match name {
                    operators::EQUALS => Ok(Self::IsNull {
                        field,
                        negated: false,
                    }),
                    operators::NOT_EQUALS => Ok(Self::IsNull {
                        field,
                        negated: true,
                    }),
                    _ => Err(Error::Filter(format!(
                        "`{field}` can only be compared to null with `==` or `!=`"
                    ))),
                };
            }

            let Compared::Value(value) = value else {
                unreachable!("null was handled above")
            };

            Ok(Self::Compare {
                field,
                operator: match name {
                    operators::EQUALS => BinaryOperator::Eq,
                    operators::NOT_EQUALS => BinaryOperator::NotEq,
                    operators::LESS => BinaryOperator::Lt,
                    operators::LESS_EQUALS => BinaryOperator::LtEq,
                    operators::GREATER => BinaryOperator::Gt,
                    _ => BinaryOperator::GtEq,
                },
                value,
            })
        }

        fn lower_in(args: &[IdedExpr]) -> Result<Self, Error> {
            let [element, list] = args else {
                return Err(Error::Filter("`in` takes a field and a list".into()));
            };

            let CelExpr::Ident(field) = &element.expr else {
                return Err(Error::Filter(
                    "the left of `in` has to be a field".to_owned(),
                ));
            };

            let CelExpr::List(list) = &list.expr else {
                return Err(Error::Filter(format!(
                    "the right of `in` has to be a list, as in `{field} in [1, 2]`"
                )));
            };

            let values = list
                .elements
                .iter()
                .map(|element| match literal(element)? {
                    Some(Compared::Value(value)) => Ok(value),
                    _ => Err(Error::Filter(format!(
                        "every item in `{field} in [...]` has to be a plain value"
                    ))),
                })
                .collect::<Result<Vec<_>, _>>()?;

            if values.is_empty() {
                return Err(Error::Filter(format!(
                    "`{field} in []` matches nothing; leave the filter out instead"
                )));
            }

            Ok(Self::In {
                field: field.clone(),
                values,
            })
        }

        fn lower_method(name: &str, target: &IdedExpr, args: &[IdedExpr]) -> Result<Self, Error> {
            let CelExpr::Ident(field) = &target.expr else {
                return Err(Error::Filter(format!(
                    "`{name}` has to be called on a field"
                )));
            };

            let first = args.first().map(literal).transpose()?.flatten();

            let Some(Compared::Value(Literal::Text(text))) = first else {
                return Err(Error::Filter(format!("`{field}.{name}` takes one string")));
            };

            let escaped = escape(&text);
            let pattern = match name {
                "startsWith" => format!("{escaped}%"),
                "endsWith" => format!("%{escaped}"),
                "contains" => format!("%{escaped}%"),
                _ => {
                    return Err(Error::Filter(format!(
                        "`{field}.{name}` is not something this filter language does; \
                         it has `startsWith`, `endsWith` and `contains`"
                    )));
                }
            };

            Ok(Self::Like {
                field: field.clone(),
                pattern,
            })
        }

        /// Renames the fields this condition names, refusing any the map lacks.
        fn resolve(&mut self, columns: &HashMap<&str, &str>) -> Result<(), Error> {
            match self {
                Self::Truthy(field)
                | Self::Compare { field, .. }
                | Self::IsNull { field, .. }
                | Self::In { field, .. }
                | Self::Like { field, .. } => {
                    let column = columns
                        .get(field.as_str())
                        .ok_or_else(|| Error::Field(field.clone()))?;
                    *field = (*column).to_owned();
                    Ok(())
                }
                Self::Not(inner) => inner.resolve(columns),
                Self::All(conditions) | Self::Any(conditions) => {
                    for condition in conditions {
                        condition.resolve(columns)?;
                    }
                    Ok(())
                }
            }
        }

        /// Writes the condition as syntax, collecting the values it binds.
        ///
        /// Placeholders are numbered from `$1` and the values are pushed in the
        /// same order, so the two stay in step once the writer renumbers them.
        fn write(&self, values: &mut Vec<Literal>) -> Expr {
            match self {
                Self::Truthy(column) => quoted(column),

                Self::Compare {
                    field,
                    operator,
                    value,
                } => Expr::BinaryOp {
                    left: Box::new(quoted(field)),
                    op: operator.clone(),
                    right: Box::new(placeholder(value.clone(), values)),
                },

                Self::IsNull { field, negated } => {
                    let column = Box::new(quoted(field));
                    if *negated {
                        Expr::IsNotNull(column)
                    } else {
                        Expr::IsNull(column)
                    }
                }

                Self::In {
                    field,
                    values: items,
                } => Expr::InList {
                    expr: Box::new(quoted(field)),
                    list: items
                        .iter()
                        .map(|item| placeholder(item.clone(), values))
                        .collect(),
                    negated: false,
                },

                Self::Like { field, pattern } => Expr::Like {
                    negated: false,
                    any: false,
                    expr: Box::new(quoted(field)),
                    pattern: Box::new(placeholder(Literal::Text(pattern.clone()), values)),
                    escape_char: Some(Box::new(Expr::Value(
                        SqlValue::SingleQuotedString(ESCAPE.to_string()).into(),
                    ))),
                },

                // `NOT` binds looser than a comparison and tighter than `AND`, so
                // only a joined condition underneath needs the parentheses.
                Self::Not(inner) => Expr::UnaryOp {
                    op: sqlparser::ast::UnaryOperator::Not,
                    expr: Box::new(match inner.write(values) {
                        joined @ Expr::BinaryOp {
                            op: BinaryOperator::And | BinaryOperator::Or,
                            ..
                        } => Expr::Nested(Box::new(joined)),
                        other => other,
                    }),
                },

                Self::All(conditions) => join(conditions, &BinaryOperator::And, values),
                Self::Any(conditions) => join(conditions, &BinaryOperator::Or, values),
            }
        }
    }

    /// Joins conditions with one operator.
    ///
    /// Only an `OR` underneath an `AND` needs parentheses, since `AND` binds
    /// tighter; everything else would only add noise to SQL someone has to read.
    fn join(
        conditions: &[Condition],
        operator: &BinaryOperator,
        values: &mut Vec<Literal>,
    ) -> Expr {
        let wrap = |expr: Expr| match operator {
            BinaryOperator::And => parenthesize(expr),
            _ => expr,
        };

        let mut written = conditions.iter().map(|condition| condition.write(values));

        let first = written
            .next()
            .unwrap_or_else(|| Expr::Value(SqlValue::Boolean(true).into()));

        written.fold(wrap(first), |left, right| Expr::BinaryOp {
            left: Box::new(left),
            op: operator.clone(),
            right: Box::new(wrap(right)),
        })
    }

    /// Takes the next placeholder, and records the value it binds.
    fn placeholder(value: Literal, values: &mut Vec<Literal>) -> Expr {
        values.push(value);
        Expr::Value(SqlValue::Placeholder(format!("${}", values.len())).into())
    }

    /// What a field was compared to.
    enum Compared {
        Null,
        Value(Literal),
    }

    /// Reads one side of a comparison, if it is a value.
    ///
    /// `None` means the node is something else -- a field, an expression -- which
    /// is not an error here, since the caller may be looking at the other side.
    /// An error means it was meant to be a value and could not be one.
    fn literal(node: &IdedExpr) -> Result<Option<Compared>, Error> {
        if let CelExpr::Call(call) = &node.expr {
            return timestamp(call);
        }

        let CelExpr::Literal(value) = &node.expr else {
            return Ok(None);
        };

        Ok(Some(match value {
            LiteralValue::Null => Compared::Null,
            LiteralValue::Boolean(value) => Compared::Value(Literal::Bool((*value).into_inner())),
            LiteralValue::Int(value) => Compared::Value(Literal::Int((*value).into_inner())),
            LiteralValue::UInt(value) => {
                let unsigned = (*value).into_inner();
                // A silent `None` here would read as "not a value" and surface as
                // a confusing complaint about the shape of the comparison.
                let signed = i64::try_from(unsigned).map_err(|_| {
                    Error::Filter(format!("`{unsigned}` is too large to compare against"))
                })?;
                Compared::Value(Literal::Int(signed))
            }
            LiteralValue::Double(value) => Compared::Value(Literal::Float((*value).into_inner())),
            LiteralValue::String(value) => {
                Compared::Value(Literal::Text(value.clone().into_inner()))
            }
            LiteralValue::Bytes(_) => return Ok(None),
        }))
    }

    /// Reads `timestamp('...')`, the one call that is a value rather than a
    /// condition.
    ///
    /// The date is parsed here rather than passed through, so `timestamp('soon')`
    /// is refused where a request is handled instead of surfacing later as a
    /// database error about a column nobody mentioned.
    #[cfg(feature = "chrono")]
    fn timestamp(call: &cel::common::ast::CallExpr) -> Result<Option<Compared>, Error> {
        if call.func_name != "timestamp" || call.target.is_some() {
            return Ok(None);
        }

        let [argument] = call.args.as_slice() else {
            return Err(Error::Filter(
                "`timestamp()` takes one string, as in `timestamp(\"2024-01-01T00:00:00Z\")`"
                    .to_owned(),
            ));
        };

        let Some(Compared::Value(Literal::Text(text))) = literal(argument)? else {
            return Err(Error::Filter(
                "`timestamp()` takes a string, not an expression".to_owned(),
            ));
        };

        let parsed = chrono::DateTime::parse_from_rfc3339(&text).map_err(|error| {
            Error::Filter(format!(
                "`timestamp(\"{text}\")` is not an RFC 3339 date: {error}"
            ))
        })?;

        Ok(Some(Compared::Value(Literal::Timestamp(
            parsed.with_timezone(&chrono::Utc),
        ))))
    }

    /// Without the `chrono` feature there is no date to fold into, so
    /// `timestamp()` is simply a call the filter language does not have.
    ///
    /// The `Result` is not wrapping anything here, and has to stay: it is the
    /// signature the other half of this pair has.
    #[cfg(not(feature = "chrono"))]
    #[allow(clippy::unnecessary_wraps)]
    fn timestamp(_call: &cel::common::ast::CallExpr) -> Result<Option<Compared>, Error> {
        Ok(None)
    }

    /// Splits a comparison into the field and what it was compared to.
    ///
    /// Either order: `readCount > 100` and `100 < readCount` both name a field and
    /// a value, and only the field may be an identifier.
    fn pair(args: &[IdedExpr], operator: &str) -> Result<(String, Compared), Error> {
        let [left, right] = args else {
            return Err(Error::Filter(format!("`{operator}` compares two things")));
        };

        if let CelExpr::Ident(field) = &left.expr
            && let Some(value) = literal(right)?
        {
            return Ok((field.clone(), value));
        }

        if let CelExpr::Ident(field) = &right.expr
            && let Some(value) = literal(left)?
        {
            return Ok((field.clone(), value));
        }

        Err(Error::Filter(
            "a comparison is a field and a value, as in `readCount > 100`".to_owned(),
        ))
    }

    /// Names a CEL node in the way a request author would recognise.
    fn describe(expr: &CelExpr) -> &'static str {
        match expr {
            CelExpr::Literal(_) => "a bare value",
            CelExpr::List(_) => "a list",
            CelExpr::Map(_) => "a map",
            CelExpr::Select(_) => "a field of a field",
            CelExpr::Struct(_) => "a struct",
            CelExpr::Comprehension(_) => "a macro",
            _ => "that",
        }
    }

    /// CEL spells its operators `_>_` and `@in`; a request author wrote `>`.
    fn readable(name: &str, _node: &IdedExpr) -> String {
        name.trim_matches('_').trim_start_matches('@').to_owned()
    }

    /// Escapes the characters `LIKE` treats as wildcards, so a request searching
    /// for `50%` finds `50%` rather than everything starting `50`.
    fn escape(text: &str) -> String {
        let mut escaped = String::with_capacity(text.len());

        for character in text.chars() {
            if matches!(character, ESCAPE | '%' | '_') {
                escaped.push(ESCAPE);
            }
            escaped.push(character);
        }

        escaped
    }
}

#[cfg(feature = "cel")]
pub use cel_filter::Filter;

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

    /// A `filter` value did not parse, or asked for something with no meaning
    /// as a SQL condition.
    Filter(String),

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
            Self::Filter(message) => write!(f, "the filter did not parse: {message}"),
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
