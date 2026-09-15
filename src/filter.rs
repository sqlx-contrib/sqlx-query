use std::collections::HashMap;

use cel::common::ast::{Expr as CelExpr, IdedExpr, LiteralValue, operators};
use cel::parser::Parser;
use sqlparser::ast::{BinaryOperator, Expr, Value as SqlValue};

use crate::Error;
use crate::writer::{FilterExpr, IntoFilterExpr, Literal, Syntax, parenthesize, quoted};

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

        let parsed = Parser::new().parse(filter).map_err(|errors| {
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
            .map(|element| match literal(element) {
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

        let Some(Compared::Value(Literal::Text(text))) = args.first().and_then(literal) else {
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
fn join(conditions: &[Condition], operator: &BinaryOperator, values: &mut Vec<Literal>) -> Expr {
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

/// Reads one side of a comparison, if it is a plain value.
fn literal(node: &IdedExpr) -> Option<Compared> {
    let CelExpr::Literal(value) = &node.expr else {
        return None;
    };

    Some(match value {
        LiteralValue::Null => Compared::Null,
        LiteralValue::Boolean(value) => Compared::Value(Literal::Bool((*value).into_inner())),
        LiteralValue::Int(value) => Compared::Value(Literal::Int((*value).into_inner())),
        LiteralValue::UInt(value) => {
            Compared::Value(Literal::Int(i64::try_from((*value).into_inner()).ok()?))
        }
        LiteralValue::Double(value) => Compared::Value(Literal::Float((*value).into_inner())),
        LiteralValue::String(value) => Compared::Value(Literal::Text(value.clone().into_inner())),
        LiteralValue::Bytes(_) => return None,
    })
}

/// Splits a comparison into the field and what it was compared to.
///
/// Either order: `readCount > 100` and `100 < readCount` both name a field and
/// a value, and only the field may be an identifier.
fn pair(args: &[IdedExpr], operator: &str) -> Result<(String, Compared), Error> {
    let [left, right] = args else {
        return Err(Error::Filter(format!("`{operator}` compares two things")));
    };

    if let (CelExpr::Ident(field), Some(value)) = (&left.expr, literal(right)) {
        return Ok((field.clone(), value));
    }

    match (&right.expr, literal(left)) {
        (CelExpr::Ident(field), Some(value)) => Ok((field.clone(), value)),
        _ => Err(Error::Filter(
            "a comparison is a field and a value, as in `readCount > 100`".to_owned(),
        )),
    }
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
