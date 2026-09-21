//! Reads a `filter` value, written in [CEL], into a [`FilterClause`] —
//! the comparison-and-boolean part of CEL: `&&`, `||`, `!`, the six
//! comparisons, `in` over a list, arithmetic, and literals. Macros,
//! comprehensions, function calls, maps and structs are refused: they
//! have no reading as a `WHERE` clause, and guessing one would be
//! inventing SQL the caller did not ask for.
//!
//! Three steps, mirroring [`OrderByClause::parse`](sqlx_query::OrderByClause::parse)
//! /[`resolve`](sqlx_query::QueryResolver::resolve)/`sql()`:
//! [`FilterClause::parse`] reads CEL into a small condition tree (still
//! naming CEL fields, not columns); [`FilterClause::resolve`] renames
//! every field against a fail-closed allow-list, the same one
//! `OrderByClause::resolve` takes; and [`FilterClause::sql`]/
//! [`FilterClause::values`] (or, for `QueryComposer::push_where`,
//! `Into<WhereClause>`) render the tree to SQL text and bind values —
//! only now, after resolution, since a literal can't become a `$N`
//! placeholder before the field names around it are real columns.
//!
//! Unlike `sqlx-contrib/sqlx-query`'s own `Filter` (which this is a port
//! of, adapted to this crate's simpler text+values `WhereClause` model
//! instead of a `sqlparser` AST), a binary expression here always
//! parenthesizes both sides rather than tracking operator precedence —
//! more parentheses than strictly necessary, but no precedence table to
//! get wrong. Same trade [`WhereClause::and`](sqlx_query::WhereClause::and)
//! already makes.
//!
//! [CEL]: https://github.com/google/cel-spec

use std::collections::HashMap;

use cel::common::ast as cel_ast;
use sqlx_query::{QueryResolver, Value, WhereClause};

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum FilterClauseError {
    #[error("a filter cannot be blank")]
    Blank,

    #[error("{0}")]
    Parse(String),

    #[error("`{0}` has no reading as a condition")]
    Unsupported(String),

    #[error("unknown filter field `{0}`")]
    UnknownField(String),
}

/// One node of a parsed filter condition — CEL's grammar, minus anything
/// with no `WHERE`-clause reading (macros, comprehensions, calls other
/// than the operators below, maps, structs).
#[derive(Debug, Clone, PartialEq)]
enum Expr {
    /// A dotted field name (`v.created_at`) — still the name the request
    /// used until [`FilterClause::resolve`] renames it to a column.
    Field(String),
    Literal(Value),
    Binary(Box<Expr>, BinOp, Box<Expr>),
    Not(Box<Expr>),
    Neg(Box<Expr>),
    IsNull(Box<Expr>),
    IsNotNull(Box<Expr>),
    InList(Box<Expr>, Vec<Expr>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BinOp {
    And,
    Or,
    Eq,
    NotEq,
    Lt,
    LtEq,
    Gt,
    GtEq,
    Add,
    Sub,
    Mul,
    Div,
    Mod,
}

impl BinOp {
    fn sql(self) -> &'static str {
        match self {
            BinOp::And => "AND",
            BinOp::Or => "OR",
            BinOp::Eq => "=",
            BinOp::NotEq => "<>",
            BinOp::Lt => "<",
            BinOp::LtEq => "<=",
            BinOp::Gt => ">",
            BinOp::GtEq => ">=",
            BinOp::Add => "+",
            BinOp::Sub => "-",
            BinOp::Mul => "*",
            BinOp::Div => "/",
            BinOp::Mod => "%",
        }
    }
}

/// A parsed, not-yet-resolved CEL filter condition. See the module docs
/// for the parse -> resolve -> render pipeline.
#[derive(Debug, Clone, PartialEq)]
pub struct FilterClause {
    expr: Expr,
}

impl FilterClause {
    /// Reads `filter` as CEL. See the module docs for the accepted
    /// grammar.
    ///
    /// `status == null` is written as `IS NULL` rather than `= NULL`,
    /// which in SQL is never true and so never what was meant.
    ///
    /// Unlike [`OrderByClause::parse`](sqlx_query::OrderByClause::parse),
    /// blank is refused rather than read as "no condition": an ordering
    /// is a list and can be empty, a condition is one expression and
    /// cannot. Skip the call instead.
    ///
    /// # Errors
    ///
    /// [`FilterClauseError::Blank`] on an empty/all-whitespace string;
    /// [`FilterClauseError::Parse`] if it isn't valid CEL;
    /// [`FilterClauseError::Unsupported`] if it parses as CEL with no
    /// reading as a condition (a macro, a map literal, a function call
    /// other than the operators above, ...).
    pub fn parse(filter: &str) -> Result<Self, FilterClauseError> {
        if filter.trim().is_empty() {
            return Err(FilterClauseError::Blank);
        }

        let parsed = cel::parser::Parser::new()
            .parse(filter)
            .map_err(|errors| FilterClauseError::Parse(errors.to_string()))?;

        Ok(FilterClause {
            expr: condition(&parsed)?,
        })
    }

    /// This condition's SQL text, `$N`-placeholder-numbered as if it were
    /// the only thing in the query — matches
    /// [`WhereClause::new`](sqlx_query::WhereClause::new)'s own
    /// numbering convention, since that's what
    /// [`Into<WhereClause>`](Self) hands it to.
    pub fn sql(&self) -> String {
        render(&self.expr).0
    }

    /// The bind values `sql()`'s placeholders reference, in declaration
    /// order.
    pub fn values(&self) -> Vec<Value> {
        render(&self.expr).1
    }
}

impl QueryResolver for FilterClause {
    type Error = FilterClauseError;

    /// Renames every field this condition names to the column it stands
    /// for, against a fail-closed allow-list: a field the map doesn't
    /// have is refused, so a request can't filter on a column it wasn't
    /// offered. Same allow-list shape as
    /// [`OrderByClause::resolve`](sqlx_query::OrderByClause::resolve).
    fn resolve(mut self, columns: &HashMap<&str, &str>) -> Result<Self, Self::Error> {
        rename(&mut self.expr, columns)?;
        Ok(self)
    }
}

impl From<FilterClause> for WhereClause {
    fn from(filter: FilterClause) -> Self {
        let (sql, values) = render(&filter.expr);
        values
            .into_iter()
            .fold(WhereClause::new(sql), WhereClause::bind)
    }
}

fn rename(expr: &mut Expr, columns: &HashMap<&str, &str>) -> Result<(), FilterClauseError> {
    match expr {
        Expr::Field(name) => match columns.get(name.as_str()) {
            Some(column) => {
                *name = (*column).to_owned();
                Ok(())
            }
            None => Err(FilterClauseError::UnknownField(name.clone())),
        },
        Expr::Literal(_) => Ok(()),
        Expr::Binary(left, _, right) => {
            rename(left, columns)?;
            rename(right, columns)
        }
        Expr::Not(inner) | Expr::Neg(inner) | Expr::IsNull(inner) | Expr::IsNotNull(inner) => {
            rename(inner, columns)
        }
        Expr::InList(needle, list) => {
            rename(needle, columns)?;
            list.iter_mut().try_for_each(|item| rename(item, columns))
        }
    }
}

/// Renders `expr` to SQL text, pushing each literal it contains as a
/// `$N` placeholder in the order encountered.
fn render(expr: &Expr) -> (String, Vec<Value>) {
    let mut values = Vec::new();
    let sql = render_into(expr, &mut values);
    (sql, values)
}

fn render_into(expr: &Expr, values: &mut Vec<Value>) -> String {
    match expr {
        Expr::Field(name) => name.clone(),
        Expr::Literal(value) => {
            values.push(value.clone());
            format!("${}", values.len())
        }
        Expr::Binary(left, op, right) => format!(
            "({}) {} ({})",
            render_into(left, values),
            op.sql(),
            render_into(right, values)
        ),
        Expr::Not(inner) => format!("NOT ({})", render_into(inner, values)),
        Expr::Neg(inner) => format!("-({})", render_into(inner, values)),
        Expr::IsNull(inner) => format!("{} IS NULL", render_into(inner, values)),
        Expr::IsNotNull(inner) => format!("{} IS NOT NULL", render_into(inner, values)),
        Expr::InList(needle, list) => {
            let items: Vec<String> = list.iter().map(|item| render_into(item, values)).collect();
            format!("{} IN ({})", render_into(needle, values), items.join(", "))
        }
    }
}

/// Reads one CEL node as the condition-tree node it stands for.
fn condition(node: &cel::IdedExpr) -> Result<Expr, FilterClauseError> {
    match &node.expr {
        cel_ast::Expr::Call(call) => call_expr(call),
        cel_ast::Expr::Literal(value) => Ok(Expr::Literal(literal(value)?)),
        cel_ast::Expr::Ident(_) | cel_ast::Expr::Select(_) => field_name(node)
            .map(Expr::Field)
            .ok_or_else(|| refused(node)),
        _ => Err(refused(node)),
    }
}

/// The dotted field a CEL identifier or selection names -- `v.created_at`.
fn field_name(node: &cel::IdedExpr) -> Option<String> {
    match &node.expr {
        cel_ast::Expr::Ident(name) => Some(name.clone()),
        // `a.b?.c` tests for presence rather than naming a field.
        cel_ast::Expr::Select(select) if !select.test => {
            Some(format!("{}.{}", field_name(&select.operand)?, select.field))
        }
        _ => None,
    }
}

/// A CEL literal, as the [`Value`] it means.
fn literal(value: &cel_ast::LiteralValue) -> Result<Value, FilterClauseError> {
    Ok(match value {
        cel_ast::LiteralValue::String(string) => Value::String(string.to_string()),
        cel_ast::LiteralValue::Boolean(boolean) => Value::Bool(**boolean),
        cel_ast::LiteralValue::Int(int) => Value::Int(**int),
        cel_ast::LiteralValue::UInt(uint) => Value::Int(**uint as i64),
        cel_ast::LiteralValue::Double(double) => Value::Float(**double),
        cel_ast::LiteralValue::Null => Value::Null,
        cel_ast::LiteralValue::Bytes(_) => {
            return Err(FilterClauseError::Unsupported(
                "a bytes literal has no SQL spelling".to_owned(),
            ));
        }
    })
}

fn call_expr(call: &cel_ast::CallExpr) -> Result<Expr, FilterClauseError> {
    use cel::common::ast::operators as cel_ops;

    let binary = |op: BinOp| -> Result<Expr, FilterClauseError> {
        let [left, right] = pair(call)?;
        Ok(Expr::Binary(
            Box::new(condition(left)?),
            op,
            Box::new(condition(right)?),
        ))
    };

    match call.func_name.as_str() {
        cel_ops::LOGICAL_AND => binary(BinOp::And),
        cel_ops::LOGICAL_OR => binary(BinOp::Or),
        cel_ops::EQUALS => comparison(call, BinOp::Eq),
        cel_ops::NOT_EQUALS => comparison(call, BinOp::NotEq),
        cel_ops::LESS => binary(BinOp::Lt),
        cel_ops::LESS_EQUALS => binary(BinOp::LtEq),
        cel_ops::GREATER => binary(BinOp::Gt),
        cel_ops::GREATER_EQUALS => binary(BinOp::GtEq),
        cel_ops::ADD => binary(BinOp::Add),
        cel_ops::SUBSTRACT => binary(BinOp::Sub),
        cel_ops::MULTIPLY => binary(BinOp::Mul),
        cel_ops::DIVIDE => binary(BinOp::Div),
        cel_ops::MODULO => binary(BinOp::Mod),
        cel_ops::IN => in_list(call),
        cel_ops::LOGICAL_NOT => Ok(Expr::Not(Box::new(condition(only(call)?)?))),
        cel_ops::NEGATE => Ok(Expr::Neg(Box::new(condition(only(call)?)?))),
        name => Err(FilterClauseError::Unsupported(name.to_owned())),
    }
}

/// `==`/`!=` against `null` are `IS NULL`/`IS NOT NULL` in SQL. Written
/// literally, `= NULL` is never true, so it is never what was meant.
fn comparison(call: &cel_ast::CallExpr, op: BinOp) -> Result<Expr, FilterClauseError> {
    let [left, right] = pair(call)?;

    let is_null = |node: &cel::IdedExpr| {
        matches!(
            node.expr,
            cel_ast::Expr::Literal(cel_ast::LiteralValue::Null)
        )
    };

    let other = match (is_null(left), is_null(right)) {
        (true, true) | (false, false) => {
            return Ok(Expr::Binary(
                Box::new(condition(left)?),
                op,
                Box::new(condition(right)?),
            ));
        }
        (true, false) => right,
        (false, true) => left,
    };

    let other = Box::new(condition(other)?);
    Ok(match op {
        BinOp::Eq => Expr::IsNull(other),
        _ => Expr::IsNotNull(other),
    })
}

fn in_list(call: &cel_ast::CallExpr) -> Result<Expr, FilterClauseError> {
    let [needle, haystack] = pair(call)?;

    let cel_ast::Expr::List(list) = &haystack.expr else {
        return Err(FilterClauseError::Unsupported(
            "`in` reads a list on its right".to_owned(),
        ));
    };

    Ok(Expr::InList(
        Box::new(condition(needle)?),
        list.elements
            .iter()
            .map(condition)
            .collect::<Result<_, _>>()?,
    ))
}

fn pair(call: &cel_ast::CallExpr) -> Result<[&cel::IdedExpr; 2], FilterClauseError> {
    match call.args.as_slice() {
        [left, right] => Ok([left, right]),
        _ => Err(FilterClauseError::Unsupported(format!(
            "`{}` reads two operands",
            call.func_name
        ))),
    }
}

fn only(call: &cel_ast::CallExpr) -> Result<&cel::IdedExpr, FilterClauseError> {
    match call.args.as_slice() {
        [only] => Ok(only),
        _ => Err(FilterClauseError::Unsupported(format!(
            "`{}` reads one operand",
            call.func_name
        ))),
    }
}

fn refused(node: &cel::IdedExpr) -> FilterClauseError {
    FilterClauseError::Unsupported(format!("{:?}", node.expr))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed(filter: &str) -> String {
        FilterClause::parse(filter).expect("condition parses").sql()
    }

    fn refuses(filter: &str) -> bool {
        matches!(
            FilterClause::parse(filter),
            Err(FilterClauseError::Parse(_) | FilterClauseError::Unsupported(_))
        )
    }

    fn columns<'a>(pairs: &[(&'a str, &'a str)]) -> HashMap<&'a str, &'a str> {
        pairs.iter().copied().collect()
    }

    #[test]
    fn a_comparison_reads_with_a_placeholder() {
        assert_eq!(parsed("rank > 3"), "(rank) > ($1)");
    }

    #[test]
    fn booleans_join_with_full_parenthesization() {
        assert_eq!(
            parsed("status == 'live' && rank > 3"),
            "((status) = ($1)) AND ((rank) > ($2))"
        );
    }

    #[test]
    fn a_field_may_be_selected_through() {
        assert_eq!(parsed("v.created_at > 3"), "(v.created_at) > ($1)");
    }

    #[test]
    fn null_is_compared_with_is_rather_than_equals() {
        assert_eq!(parsed("deleted_at == null"), "deleted_at IS NULL");
        assert_eq!(parsed("deleted_at != null"), "deleted_at IS NOT NULL");
        assert_eq!(parsed("null == deleted_at"), "deleted_at IS NULL");
    }

    #[test]
    fn in_reads_as_in_a_list() {
        assert_eq!(parsed("status in ['live', 'draft']"), "status IN ($1, $2)");
        assert_eq!(
            FilterClause::parse("status in ['live', 'draft']")
                .unwrap()
                .values(),
            vec![Value::String("live".into()), Value::String("draft".into())]
        );
    }

    #[test]
    fn not_keeps_what_it_applies_to_together() {
        assert_eq!(
            parsed("!(a == 1 && b == 2)"),
            "NOT (((a) = ($1)) AND ((b) = ($2)))"
        );
    }

    #[test]
    fn what_has_no_reading_as_a_condition_is_refused() {
        assert!(matches!(
            FilterClause::parse(""),
            Err(FilterClauseError::Blank)
        ));
        assert!(matches!(
            FilterClause::parse("   "),
            Err(FilterClauseError::Blank)
        ));
        assert!(refuses("size(name) > 3"));
        assert!(refuses("[1, 2].all(x, x > 0)"));
        assert!(refuses("{'a': 1}"));
        assert!(refuses("status =="));
        assert!(refuses("status == 'live'; DROP TABLE users"));
    }

    #[test]
    fn resolve_renames_every_field_against_the_allow_list() {
        let filter = FilterClause::parse("status == 'live' && v.rank > 3")
            .expect("condition parses")
            .resolve(&columns(&[("status", "state"), ("v.rank", "v.rank_score")]))
            .expect("every field is named");

        assert_eq!(filter.sql(), "((state) = ($1)) AND ((v.rank_score) > ($2))");
    }

    #[test]
    fn resolve_refuses_a_condition_naming_an_unoffered_field() {
        let refused = FilterClause::parse("status == 'live' && secret == 1")
            .expect("condition parses")
            .resolve(&columns(&[("status", "state")]));

        assert_eq!(
            refused,
            Err(FilterClauseError::UnknownField("secret".to_owned()))
        );
    }

    #[test]
    fn into_where_clause_binds_every_literal_in_order() {
        let where_by: WhereClause = FilterClause::parse("status == 'live' && rank > 3")
            .expect("condition parses")
            .into();

        assert_eq!(
            where_by.sql().as_str(),
            "((status) = ($1)) AND ((rank) > ($2))"
        );
        assert_eq!(
            where_by.values(),
            &[Value::String("live".into()), Value::Int(3)]
        );
    }
}
