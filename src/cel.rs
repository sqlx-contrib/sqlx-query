//! Lowering a CEL expression to SQL.

use cel::common::ast::{Expr, IdedExpr, LiteralValue, operators};
use chrono::{DateTime, Utc};

use crate::dialect::{Dialect, reference};
use crate::error::Error;
use crate::fragment::QueryFragment;
use crate::mapping::{Column, ColumnType, Mapping};
use crate::value::Value;

/// The character that escapes a `LIKE` wildcard.
///
/// Not a backslash: MySQL's `NO_BACKSLASH_ESCAPES` and the backslash's own
/// double meaning in string literals make it the one character guaranteed to be
/// argued about. `!` is inert everywhere and declared explicitly with `ESCAPE`.
const LIKE_ESCAPE: char = '!';

/// Parse CEL source.
///
/// # Errors
///
/// [`Error::Parse`] for anything the CEL grammar rejects.
pub(crate) fn parse(source: &str) -> Result<IdedExpr, Error> {
    cel::parser::Parser::new()
        .parse(source)
        .map_err(|errors| Error::Parse(errors.to_string()))
}

/// Lower a parsed expression to a boolean fragment.
///
/// # Errors
///
/// [`Error::UnknownColumn`], [`Error::TypeMismatch`], or [`Error::Unsupported`]
/// for a construct with no faithful SQL lowering.
pub(crate) fn render<DB: Dialect, S: Mapping>(
    expr: &IdedExpr,
    mapping: &S,
) -> Result<QueryFragment<DB, Value>, Error> {
    let mut fragment = QueryFragment::new();
    condition(expr, mapping, &mut fragment)?;
    Ok(fragment)
}

/// Write `expr` as a SQL boolean expression.
fn condition<DB: Dialect, S: Mapping>(
    expr: &IdedExpr,
    mapping: &S,
    out: &mut QueryFragment<DB, Value>,
) -> Result<(), Error> {
    match &expr.expr {
        // `has(x)` reaches us as a select marked `test`. In SQL the question
        // "is this field set" is "is this column not null".
        Expr::Select(select) if select.test => {
            let column = column_of(&IdedExpr {
                id: expr.id,
                expr: Expr::Select(cel::common::ast::SelectExpr {
                    test: false,
                    ..select.clone()
                }),
            })
            .ok_or_else(|| unsupported("has() over something that is not a field"))?;

            let column = resolve(mapping, &column)?;
            out.push(&reference::<DB>(&column));
            out.push(" IS NOT NULL");
            Ok(())
        }

        Expr::Call(call) => call_condition(call, mapping, out),

        Expr::Literal(LiteralValue::Boolean(value)) => {
            out.push(if **value { "TRUE" } else { "FALSE" });
            Ok(())
        }

        // A bare column is only a condition if it is boolean, and SQL is happy
        // to take one directly.
        Expr::Ident(_) | Expr::Select(_) => {
            let path = column_of(expr).ok_or_else(|| unsupported("this expression"))?;
            let column = resolve(mapping, &path)?;

            if column.ty != ColumnType::Bool {
                return Err(Error::TypeMismatch(format!(
                    "`{path}` is {} and cannot stand alone as a condition",
                    column.ty
                )));
            }

            out.push(&reference::<DB>(&column));
            Ok(())
        }

        Expr::Comprehension(_) => Err(unsupported(
            "a comprehension macro (`all`, `exists`, `map`, `filter`): these \
             iterate, and a WHERE clause cannot",
        )),
        Expr::Map(_) | Expr::Struct(_) => Err(unsupported("a map or struct literal")),
        _ => Err(unsupported("this expression as a condition")),
    }
}

fn call_condition<DB: Dialect, S: Mapping>(
    call: &cel::common::ast::CallExpr,
    mapping: &S,
    out: &mut QueryFragment<DB, Value>,
) -> Result<(), Error> {
    let name = call.func_name.as_str();

    match name {
        operators::LOGICAL_AND | operators::LOGICAL_OR => {
            let [left, right] = pair(&call.args, name)?;
            let joiner = if name == operators::LOGICAL_AND {
                " AND "
            } else {
                " OR "
            };

            out.push("(");
            condition(left, mapping, out)?;
            out.push(joiner);
            condition(right, mapping, out)?;
            out.push(")");
            Ok(())
        }

        operators::LOGICAL_NOT => {
            let [only] = single(&call.args, name)?;

            out.push("(NOT ");
            condition(only, mapping, out)?;
            out.push(")");
            Ok(())
        }

        operators::EQUALS
        | operators::NOT_EQUALS
        | operators::LESS
        | operators::LESS_EQUALS
        | operators::GREATER
        | operators::GREATER_EQUALS => {
            let [left, right] = pair(&call.args, name)?;
            comparison(name, left, right, mapping, out)
        }

        operators::IN => {
            let [needle, haystack] = pair(&call.args, name)?;
            membership(needle, haystack, mapping, out)
        }

        "startsWith" | "endsWith" | "contains" => like(call, name, mapping, out),

        operators::CONDITIONAL => Err(unsupported(
            "the ternary operator: write it as `(a && b) || (!a && c)`",
        )),

        _ => Err(unsupported(&format!("the function `{name}`"))),
    }
}

/// One side of a comparison, once we know what it is.
enum Operand {
    Column(Column, String),
    Value(Value),
    Null,
}

fn comparison<DB: Dialect, S: Mapping>(
    operator: &str,
    left: &IdedExpr,
    right: &IdedExpr,
    mapping: &S,
    out: &mut QueryFragment<DB, Value>,
) -> Result<(), Error> {
    let left = operand(left, mapping)?;
    let right = operand(right, mapping)?;

    match (left, right) {
        // `x == null` is `IS NULL`, which is both what the caller means and the
        // only form that behaves: a bound NULL makes a comparison unknown.
        (Operand::Column(column, _), Operand::Null)
        | (Operand::Null, Operand::Column(column, _)) => {
            let sql = match operator {
                operators::EQUALS => " IS NULL",
                operators::NOT_EQUALS => " IS NOT NULL",
                _ => {
                    return Err(unsupported(
                        "ordering a column against null: only `==` and `!=` mean anything",
                    ));
                }
            };

            out.push(&reference::<DB>(&column));
            out.push(sql);
            Ok(())
        }

        (Operand::Column(column, path), Operand::Value(value)) => {
            check(&column, &value, &path)?;

            out.push(&reference::<DB>(&column));
            out.push(sql_operator(operator)?);
            out.push_bind(value);
            Ok(())
        }

        // Flipped, so `21 < age` reads as `age > 21` rather than binding on the
        // left of the operator.
        (Operand::Value(value), Operand::Column(column, path)) => {
            check(&column, &value, &path)?;

            out.push(&reference::<DB>(&column));
            out.push(sql_operator(flip(operator))?);
            out.push_bind(value);
            Ok(())
        }

        (Operand::Column(left, left_path), Operand::Column(right, right_path)) => {
            if left.ty != right.ty {
                return Err(Error::TypeMismatch(format!(
                    "`{left_path}` is {} and `{right_path}` is {}",
                    left.ty, right.ty
                )));
            }

            out.push(&reference::<DB>(&left));
            out.push(sql_operator(operator)?);
            out.push(&reference::<DB>(&right));
            Ok(())
        }

        (Operand::Null, _) | (_, Operand::Null) => Err(unsupported("comparing null to null")),

        (Operand::Value(_), Operand::Value(_)) => Err(unsupported(
            "a comparison between two constants: it names no column, so it \
             filters nothing",
        )),
    }
}

fn membership<DB: Dialect, S: Mapping>(
    needle: &IdedExpr,
    haystack: &IdedExpr,
    mapping: &S,
    out: &mut QueryFragment<DB, Value>,
) -> Result<(), Error> {
    let Operand::Column(column, path) = operand(needle, mapping)? else {
        return Err(unsupported("`in` over something that is not a column"));
    };

    let Expr::List(list) = &haystack.expr else {
        return Err(unsupported(
            "`in` over something that is not a list literal",
        ));
    };

    if list.elements.is_empty() {
        // `IN ()` is a syntax error everywhere, and the answer is known anyway.
        out.push("FALSE");
        return Ok(());
    }

    out.push(&reference::<DB>(&column));
    out.push(" IN (");

    for (at, element) in list.elements.iter().enumerate() {
        if at > 0 {
            out.push(", ");
        }

        let Operand::Value(value) = operand(element, mapping)? else {
            return Err(unsupported("a non-constant element in an `in` list"));
        };

        check(&column, &value, &path)?;
        out.push_bind(value);
    }

    out.push(")");
    Ok(())
}

fn like<DB: Dialect, S: Mapping>(
    call: &cel::common::ast::CallExpr,
    name: &str,
    mapping: &S,
    out: &mut QueryFragment<DB, Value>,
) -> Result<(), Error> {
    let target = call
        .target
        .as_deref()
        .ok_or_else(|| unsupported(&format!("`{name}` without a receiver")))?;

    let Operand::Column(column, path) = operand(target, mapping)? else {
        return Err(unsupported(&format!(
            "`{name}` on something that is not a column"
        )));
    };

    if column.ty != ColumnType::Text {
        return Err(Error::TypeMismatch(format!(
            "`{path}` is {} and has no text to match",
            column.ty
        )));
    }

    let [argument] = single(&call.args, name)?;
    let Operand::Value(Value::Text(needle)) = operand(argument, mapping)? else {
        return Err(unsupported(&format!(
            "`{name}` with an argument that is not a string constant"
        )));
    };

    let escaped = escape_like(&needle);
    let pattern = match name {
        "startsWith" => format!("{escaped}%"),
        "endsWith" => format!("%{escaped}"),
        _ => format!("%{escaped}%"),
    };

    out.push(&reference::<DB>(&column));
    out.push(" LIKE ");
    out.push_bind(Value::Text(pattern));
    out.push(&format!(" ESCAPE '{LIKE_ESCAPE}'"));
    Ok(())
}

/// Resolve one side of a comparison.
fn operand<S: Mapping>(expr: &IdedExpr, mapping: &S) -> Result<Operand, Error> {
    if let Some(path) = column_of(expr) {
        let column = resolve(mapping, &path)?;
        return Ok(Operand::Column(column, path));
    }

    match &expr.expr {
        Expr::Literal(LiteralValue::Null) => Ok(Operand::Null),
        Expr::Literal(literal) => Ok(Operand::Value(literal_value(literal)?)),
        Expr::Call(call) if call.func_name == "timestamp" && call.target.is_none() => {
            let [argument] = single(&call.args, "timestamp")?;

            let Expr::Literal(LiteralValue::String(text)) = &argument.expr else {
                return Err(unsupported("`timestamp()` with a non-constant argument"));
            };

            let parsed: DateTime<Utc> = text.parse::<DateTime<Utc>>().map_err(|error| {
                Error::Parse(format!("`{}` is not a timestamp: {error}", &**text))
            })?;

            Ok(Operand::Value(Value::Timestamp(parsed)))
        }
        _ => Err(unsupported("this expression as a value")),
    }
}

fn literal_value(literal: &LiteralValue) -> Result<Value, Error> {
    Ok(match literal {
        LiteralValue::Boolean(value) => Value::Bool(**value),
        LiteralValue::Int(value) => Value::Int(**value),
        LiteralValue::UInt(value) => Value::Int(i64::try_from(**value).map_err(|_| {
            Error::TypeMismatch(format!("`{}` does not fit in a signed integer", **value))
        })?),
        LiteralValue::Double(value) => Value::Float(**value),
        LiteralValue::String(value) => Value::Text((**value).to_owned()),
        LiteralValue::Bytes(value) => Value::Bytes((**value).to_vec()),
        LiteralValue::Null => return Err(unsupported("null here")),
    })
}

/// The dotted path this expression names, if it names one.
fn column_of(expr: &IdedExpr) -> Option<String> {
    let mut segments = Vec::new();
    walk(expr, &mut segments)?;
    Some(segments.join("."))
}

fn walk(expr: &IdedExpr, into: &mut Vec<String>) -> Option<()> {
    match &expr.expr {
        Expr::Ident(name) => {
            into.push(name.clone());
            Some(())
        }
        Expr::Select(select) if !select.test => {
            walk(&select.operand, into)?;
            into.push(select.field.clone());
            Some(())
        }
        _ => None,
    }
}

fn resolve<S: Mapping>(mapping: &S, path: &str) -> Result<Column, Error> {
    let segments: Vec<&str> = path.split('.').collect();

    mapping
        .resolve(&segments)
        .ok_or_else(|| Error::UnknownColumn(path.to_owned()))
}

/// Whether a literal can be compared against a column.
///
/// An integer literal against a float column is allowed, because `price > 10`
/// is what people write and the widening is exact. Nothing else crosses.
fn check(column: &Column, value: &Value, path: &str) -> Result<(), Error> {
    let given = value.ty();

    let ok = column.ty == given || (column.ty == ColumnType::Float && given == ColumnType::Int);

    if ok {
        return Ok(());
    }

    Err(Error::TypeMismatch(format!(
        "`{path}` is {} but was compared against {given}",
        column.ty
    )))
}

fn sql_operator(operator: &str) -> Result<&'static str, Error> {
    Ok(match operator {
        operators::EQUALS => " = ",
        operators::NOT_EQUALS => " <> ",
        operators::LESS => " < ",
        operators::LESS_EQUALS => " <= ",
        operators::GREATER => " > ",
        operators::GREATER_EQUALS => " >= ",
        _ => return Err(unsupported(&format!("the operator `{operator}`"))),
    })
}

/// The operator that means the same thing with its operands swapped.
fn flip(operator: &str) -> &str {
    match operator {
        operators::LESS => operators::GREATER,
        operators::LESS_EQUALS => operators::GREATER_EQUALS,
        operators::GREATER => operators::LESS,
        operators::GREATER_EQUALS => operators::LESS_EQUALS,
        same => same,
    }
}

/// Neutralise the wildcards in a `LIKE` pattern.
fn escape_like(needle: &str) -> String {
    let mut out = String::with_capacity(needle.len());

    for character in needle.chars() {
        if matches!(character, LIKE_ESCAPE | '%' | '_') {
            out.push(LIKE_ESCAPE);
        }
        out.push(character);
    }

    out
}

fn pair<'a>(args: &'a [IdedExpr], name: &str) -> Result<[&'a IdedExpr; 2], Error> {
    match args {
        [left, right] => Ok([left, right]),
        _ => Err(unsupported(&format!(
            "`{name}` with {} arguments",
            args.len()
        ))),
    }
}

fn single<'a>(args: &'a [IdedExpr], name: &str) -> Result<[&'a IdedExpr; 1], Error> {
    match args {
        [only] => Ok([only]),
        _ => Err(unsupported(&format!(
            "`{name}` with {} arguments",
            args.len()
        ))),
    }
}

fn unsupported(what: &str) -> Error {
    Error::Unsupported(format!("no SQL lowering for {what}"))
}

#[cfg(test)]
mod tests {
    use sqlx::Postgres;

    use super::*;
    use crate::mapping::{ColumnType, QueryMapping};

    fn volumes() -> QueryMapping {
        QueryMapping::new()
            .key("id", ColumnType::Int)
            .column("title", ColumnType::Text)
            .column("price", ColumnType::Float)
            .column("archived", ColumnType::Bool)
            .column("publishedAt", ColumnType::Timestamp)
            .add("author.name", Column::new("author_name", ColumnType::Text))
    }

    fn sql(source: &str) -> String {
        render::<Postgres, _>(&parse(source).unwrap(), &volumes())
            .unwrap()
            .preview()
    }

    fn error(source: &str) -> Error {
        render::<Postgres, _>(&parse(source).unwrap(), &volumes()).unwrap_err()
    }

    #[test]
    fn comparisons_bind_their_constant() {
        assert_eq!(sql("id > 21"), r#""id" > ?"#);
        assert_eq!(sql("title == 'Dune'"), r#""title" = ?"#);
        assert_eq!(sql("id != 3"), r#""id" <> ?"#);
    }

    /// The column belongs on the left whichever way it was written.
    #[test]
    fn a_reversed_comparison_flips_its_operator() {
        assert_eq!(sql("21 < id"), r#""id" > ?"#);
        assert_eq!(sql("21 >= id"), r#""id" <= ?"#);
    }

    #[test]
    fn logic_nests_with_parentheses() {
        assert_eq!(
            sql("id > 1 && (title == 'Dune' || archived)"),
            r#"("id" > ? AND ("title" = ? OR "archived"))"#
        );
        assert_eq!(sql("!archived"), r#"(NOT "archived")"#);
    }

    /// A bound NULL makes a comparison unknown rather than true, so the only
    /// forms that mean what the caller wrote take no parameter.
    #[test]
    fn null_becomes_is_null() {
        assert_eq!(sql("publishedAt == null"), r#""publishedAt" IS NULL"#);
        assert_eq!(sql("publishedAt != null"), r#""publishedAt" IS NOT NULL"#);
    }

    #[test]
    fn has_asks_whether_a_column_is_set() {
        assert_eq!(sql("has(author.name)"), r#""author_name" IS NOT NULL"#);
    }

    #[test]
    fn dotted_paths_resolve_through_the_schema() {
        assert_eq!(sql("author.name == 'Herbert'"), r#""author_name" = ?"#);
    }

    #[test]
    fn the_string_functions_become_escaped_like() {
        assert_eq!(
            sql("title.startsWith('Du')"),
            r#""title" LIKE ? ESCAPE '!'"#
        );
        assert_eq!(sql("title.contains('un')"), r#""title" LIKE ? ESCAPE '!'"#);
    }

    /// A caller searching for a literal `%` must not get every row.
    #[test]
    fn like_wildcards_in_the_needle_are_neutralised() {
        let fragment =
            render::<Postgres, _>(&parse("title.startsWith('100%_x')").unwrap(), &volumes())
                .unwrap();

        let (_, values) = fragment.parts_for_test();
        assert_eq!(values, [Value::Text("100!%!_x%".into())]);
    }

    #[test]
    fn in_becomes_a_bound_list() {
        assert_eq!(sql("id in [1, 2, 3]"), r#""id" IN (?, ?, ?)"#);
    }

    /// `IN ()` is a syntax error everywhere, and the answer is known.
    #[test]
    fn an_empty_in_list_is_false() {
        assert_eq!(sql("id in []"), "FALSE");
    }

    #[test]
    fn timestamps_fold_into_a_bind_value() {
        assert_eq!(
            sql("publishedAt > timestamp('2024-01-01T00:00:00Z')"),
            r#""publishedAt" > ?"#
        );
    }

    /// cel-rust parses without checking, so the mapping is the only thing that
    /// can catch this before the database does.
    #[test]
    fn a_type_mismatch_is_rejected() {
        assert!(matches!(error("id > 'tuesday'"), Error::TypeMismatch(_)));
        assert!(matches!(error("title > 3"), Error::TypeMismatch(_)));
    }

    /// Integers against floats are the one crossing, because `price > 10` is
    /// what people write and widening is exact.
    #[test]
    fn an_integer_may_be_compared_against_a_float_column() {
        assert_eq!(sql("price > 10"), r#""price" > ?"#);
    }

    #[test]
    fn an_unknown_column_is_rejected() {
        assert!(matches!(error("salary > 1"), Error::UnknownColumn(f) if f == "salary"));
    }

    #[test]
    fn constructs_with_no_lowering_are_rejected_not_approximated() {
        for source in [
            "[1, 2].all(x, x > 1)",
            "{'a': 1}.a == 1",
            "id > 1 ? true : false",
            "id + 1 > 2",
            "1 > 0",
        ] {
            assert!(
                matches!(error(source), Error::Unsupported(_)),
                "accepted `{source}`"
            );
        }
    }

    #[test]
    fn a_non_boolean_column_cannot_stand_alone() {
        assert!(matches!(error("title"), Error::TypeMismatch(_)));
    }
}
