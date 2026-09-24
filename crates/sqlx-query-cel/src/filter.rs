//! Reads a `filter` value, written in [CEL], into a [`FilterClause`] —
//! the comparison-and-boolean part of CEL: `&&`, `||`, `!`, the six
//! comparisons, `in` over a list, arithmetic, and literals — plus the
//! string methods `startsWith`, `endsWith` and `contains`, which read as
//! `LIKE`, and `timestamp("...")` and `uuid("...")`, which read a string
//! as a timestamp or a UUID literal. Macros, comprehensions, other function calls, maps
//! and structs are refused: they have no reading as a `WHERE` clause, and
//! guessing one would be inventing SQL the caller did not ask for.
//!
//! [`FilterClause`] holds the parsed CEL tree itself — no separate
//! condition-tree type mirroring it, since that would just be this
//! crate's own version of what `sqlx-contrib/sqlx-query`'s `Filter` does
//! with a `sqlparser` AST (which this is a port of, minus the AST
//! dependency). [`FilterClause::parse`] validates the tree once by
//! rendering it and discarding the result; [`FilterClause::resolve`]
//! mutates the tree in place, collapsing each resolved field reference
//! into a flat `Ident` node carrying the real column text; and
//! [`FilterClause::to_where_clause`] (or plain `Into<WhereClause>`, for
//! `QueryComposer::push_where`) renders the (now-resolved) tree straight
//! into a [`WhereClause`], matching
//! [`Cursor::to_where_clause`](sqlx_query::Cursor::to_where_clause)'s
//! naming.
//!
//! A binary expression always parenthesizes both sides rather than
//! tracking operator precedence — more parentheses than strictly
//! necessary, but no precedence table to get wrong. Same trade
//! [`WhereClause::and`](sqlx_query::WhereClause::and) already makes.
//!
//! [CEL]: https://github.com/google/cel-spec

use std::collections::HashMap;

use cel::common::ast::{CallExpr, Expr, LiteralValue};
use sqlx_query::{QueryResolver, Value, WhereClause};

/// Errors [`FilterClause::parse`]/[`FilterClause::resolve`] can return.
#[derive(Debug, thiserror::Error, PartialEq)]
pub enum FilterClauseError {
    #[error("{0}")]
    Parse(String),

    #[error("`{0}` has no reading as a condition")]
    Unsupported(String),

    /// A literal that does not read as the type a function asks for:
    /// `timestamp("yesterday")`, `uuid("x")`.
    #[error("{0}")]
    InvalidLiteral(String),

    #[error("unknown filter field `{0}`")]
    UnknownField(String),
}

/// A parsed, not-yet-resolved CEL filter condition. See the module docs
/// for the parse -> resolve -> render pipeline.
///
/// The default is the empty filter — what a blank string parses to — which
/// has no condition: it resolves to itself and renders as an empty
/// [`WhereClause`], so a composer leaves its slot empty.
#[derive(Debug, Clone, Default)]
pub struct FilterClause {
    /// `None` for the empty filter.
    expr: Option<cel::IdedExpr>,
}

impl FilterClause {
    /// Reads `filter` as CEL. See the module docs for the accepted
    /// grammar.
    ///
    /// `status == null` is written as `IS NULL` rather than `= NULL`,
    /// which in SQL is never true and so never what was meant.
    ///
    /// An empty or all-whitespace string is the empty filter, no condition
    /// at all — as [`OrderByClause::parse`](sqlx_query::OrderByClause::parse)
    /// reads a blank ordering, and as AIP-160 reads a blank `filter`.
    ///
    /// # Errors
    ///
    /// [`FilterClauseError::Parse`] if it isn't valid CEL;
    /// [`FilterClauseError::Unsupported`] if it parses as CEL with no
    /// reading as a condition (a macro, a map literal, a function call
    /// other than the operators above, ...).
    pub fn parse(filter: &str) -> Result<Self, FilterClauseError> {
        if filter.trim().is_empty() {
            return Ok(FilterClause::default());
        }

        let parsed = cel::parser::Parser::new()
            .parse(filter)
            .map_err(|errors| FilterClauseError::Parse(errors.to_string()))?;

        // Fails fast on anything with no reading as a condition.
        Self::validate(&parsed)?;

        Ok(FilterClause { expr: Some(parsed) })
    }

    /// This condition as a [`WhereClause`], with every literal bound to
    /// a `$N` placeholder rather than spliced into the text — matches
    /// [`Cursor::to_where_clause`](sqlx_query::Cursor::to_where_clause)'s
    /// naming, since both types answer "what's your WHERE-clause form?"
    /// the same way. The empty filter is the empty clause.
    ///
    /// # Panics
    ///
    /// If this tree doesn't render — unreachable, since
    /// [`parse`](Self::parse) rejects one that doesn't before a
    /// `FilterClause` exists to call this on.
    #[must_use]
    pub fn to_where_clause(&self) -> WhereClause {
        let Some(expr) = &self.expr else {
            return WhereClause::default();
        };

        let mut values = Vec::new();
        let sql =
            Self::render(expr, &mut values).expect("parse() already validated this tree renders");
        values
            .into_iter()
            .fold(WhereClause::new(sql), WhereClause::bind_value)
    }

    /// Confirms `node` has a reading as a condition, without building
    /// the SQL text/values `render` would — there's nothing to do with
    /// them yet at parse time, before fields are resolved to columns.
    fn validate(node: &cel::IdedExpr) -> Result<(), FilterClauseError> {
        Self::render(node, &mut Vec::new()).map(|_| ())
    }

    /// Renders `node` to SQL text, pushing each literal it contains onto
    /// `values` as a `$N` placeholder in the order encountered — always
    /// threaded through the same accumulator, since every literal in a
    /// condition shares one placeholder numbering.
    fn render(node: &cel::IdedExpr, values: &mut Vec<Value>) -> Result<String, FilterClauseError> {
        match &node.expr {
            Expr::Call(call) => Self::render_call(call, values),
            Expr::Literal(value) => {
                values.push(Self::literal(value)?);
                Ok(format!("${}", values.len()))
            }
            Expr::Ident(_) | Expr::Select(_) => {
                Self::field_name(node).ok_or_else(|| Self::refused(node))
            }
            _ => Err(Self::refused(node)),
        }
    }

    fn render_call(call: &CallExpr, values: &mut Vec<Value>) -> Result<String, FilterClauseError> {
        use cel::common::ast::operators as cel_ops;

        match call.func_name.as_str() {
            cel_ops::LOGICAL_AND => Self::render_binary(call, "AND", values),
            cel_ops::LOGICAL_OR => Self::render_binary(call, "OR", values),
            cel_ops::EQUALS => Self::render_comparison(call, true, values),
            cel_ops::NOT_EQUALS => Self::render_comparison(call, false, values),
            cel_ops::LESS => Self::render_binary(call, "<", values),
            cel_ops::LESS_EQUALS => Self::render_binary(call, "<=", values),
            cel_ops::GREATER => Self::render_binary(call, ">", values),
            cel_ops::GREATER_EQUALS => Self::render_binary(call, ">=", values),
            cel_ops::ADD => Self::render_binary(call, "+", values),
            cel_ops::SUBSTRACT => Self::render_binary(call, "-", values),
            cel_ops::MULTIPLY => Self::render_binary(call, "*", values),
            cel_ops::DIVIDE => Self::render_binary(call, "/", values),
            cel_ops::MODULO => Self::render_binary(call, "%", values),
            cel_ops::IN => Self::render_in_list(call, values),
            cel_ops::LOGICAL_NOT => Ok(format!(
                "NOT ({})",
                Self::render(Self::only(call)?, values)?
            )),
            cel_ops::NEGATE => Ok(format!("-({})", Self::render(Self::only(call)?, values)?)),
            "startsWith" | "endsWith" | "contains" => Self::render_like(call, values),
            "timestamp" => {
                values.push(Self::timestamp(Self::constant(call)?)?);
                Ok(format!("${}", values.len()))
            }
            "uuid" => {
                values.push(Self::uuid(Self::constant(call)?)?);
                Ok(format!("${}", values.len()))
            }
            name => Err(FilterClauseError::Unsupported(format!(
                "`{name}` has no reading as a condition"
            ))),
        }
    }

    fn render_binary(
        call: &CallExpr,
        op: &str,
        values: &mut Vec<Value>,
    ) -> Result<String, FilterClauseError> {
        let [left, right] = Self::pair(call)?;
        let left_sql = Self::render(left, values)?;
        let right_sql = Self::render(right, values)?;
        Ok(format!("({left_sql}) {op} ({right_sql})"))
    }

    /// `==`/`!=` against `null` are `IS NULL`/`IS NOT NULL` in SQL.
    /// Written literally, `= NULL` is never true, so it is never what
    /// was meant.
    fn render_comparison(
        call: &CallExpr,
        is_eq: bool,
        values: &mut Vec<Value>,
    ) -> Result<String, FilterClauseError> {
        let [left, right] = Self::pair(call)?;

        let is_null = |node: &cel::IdedExpr| matches!(node.expr, Expr::Literal(LiteralValue::Null));

        match (is_null(left), is_null(right)) {
            (true, true) | (false, false) => {
                Self::render_binary(call, if is_eq { "=" } else { "<>" }, values)
            }
            (true, false) => {
                let sql = Self::render(right, values)?;
                Ok(format!(
                    "{sql} {}",
                    if is_eq { "IS NULL" } else { "IS NOT NULL" }
                ))
            }
            (false, true) => {
                let sql = Self::render(left, values)?;
                Ok(format!(
                    "{sql} {}",
                    if is_eq { "IS NULL" } else { "IS NOT NULL" }
                ))
            }
        }
    }

    /// `field.startsWith("x")`, `endsWith` and `contains` as `LIKE`, the
    /// argument bound as the pattern with its own `%`, `_` and `!` escaped so
    /// they match themselves.
    ///
    /// Only on a field, and only with a string literal: that is what has a
    /// reading as a `LIKE`, and it keeps the pattern a bound value rather
    /// than anything rendered into the text.
    ///
    /// `!` is the escape rather than `\`. A backslash inside a string literal
    /// is itself an escape in MySQL's default mode, so `ESCAPE '\'` would not
    /// read the same in every dialect; `!` means nothing in any of them.
    fn render_like(call: &CallExpr, values: &mut Vec<Value>) -> Result<String, FilterClauseError> {
        let name = &call.func_name;
        let target = call
            .target
            .as_deref()
            .filter(|target| matches!(target.expr, Expr::Ident(_) | Expr::Select(_)))
            .ok_or_else(|| {
                FilterClauseError::Unsupported(format!(
                    "`{name}` is called on a field, as `field.{name}(\"...\")`"
                ))
            })?;
        let Expr::Literal(LiteralValue::String(text)) = &Self::only(call)?.expr else {
            return Err(FilterClauseError::Unsupported(format!(
                "`{name}` reads a string literal"
            )));
        };

        let field = Self::render(target, values)?;
        let mut escaped = String::with_capacity(text.len());
        for character in text.chars() {
            if matches!(character, '!' | '%' | '_') {
                escaped.push('!');
            }
            escaped.push(character);
        }
        let pattern = match name.as_str() {
            "startsWith" => format!("{escaped}%"),
            "endsWith" => format!("%{escaped}"),
            _ => format!("%{escaped}%"),
        };

        values.push(Value::String(pattern));
        Ok(format!("({field}) LIKE ${} ESCAPE '!'", values.len()))
    }

    /// The string literal a constructor -- `timestamp("...")`, `uuid("...")` -- is called
    /// with. A global call, not a method, and on a literal only: the value
    /// is read here, at parse time, so it has to be one.
    fn constant(call: &CallExpr) -> Result<&str, FilterClauseError> {
        let name = &call.func_name;
        let (None, [argument]) = (&call.target, call.args.as_slice()) else {
            return Err(FilterClauseError::Unsupported(format!(
                "`{name}` is called as `{name}(\"...\")`"
            )));
        };
        match &argument.expr {
            Expr::Literal(LiteralValue::String(text)) => Ok(&**text),
            _ => Err(FilterClauseError::Unsupported(format!(
                "`{name}` reads a string literal"
            ))),
        }
    }

    /// `text`, an RFC 3339 timestamp, as the timestamp it names.
    #[cfg(feature = "chrono")]
    fn timestamp(text: &str) -> Result<Value, FilterClauseError> {
        chrono::DateTime::parse_from_rfc3339(text)
            .map(|timestamp| Value::from(timestamp.to_utc()))
            .map_err(|error| {
                FilterClauseError::InvalidLiteral(format!(
                    "`timestamp(\"{text}\")` is not an RFC 3339 timestamp: {error}"
                ))
            })
    }

    #[cfg(all(feature = "time", not(feature = "chrono")))]
    fn timestamp(text: &str) -> Result<Value, FilterClauseError> {
        time::OffsetDateTime::parse(text, &time::format_description::well_known::Rfc3339)
            .map(Value::from)
            .map_err(|error| {
                FilterClauseError::InvalidLiteral(format!(
                    "`timestamp(\"{text}\")` is not an RFC 3339 timestamp: {error}"
                ))
            })
    }

    /// Without a date library there is nothing to read the string with, so
    /// the function is refused rather than bound as text a timestamp column
    /// would reject.
    #[cfg(not(any(feature = "chrono", feature = "time")))]
    fn timestamp(_: &str) -> Result<Value, FilterClauseError> {
        Err(FilterClauseError::Unsupported(
            "`timestamp` needs the `chrono` or `time` feature".to_owned(),
        ))
    }

    /// `text` as the UUID it spells, in any of the forms `uuid::Uuid` reads:
    /// hyphenated, simple, braced or URN.
    #[cfg(feature = "uuid")]
    fn uuid(text: &str) -> Result<Value, FilterClauseError> {
        uuid::Uuid::parse_str(text)
            .map(Value::from)
            .map_err(|error| {
                FilterClauseError::InvalidLiteral(format!(
                    "`uuid(\"{text}\")` is not a UUID: {error}"
                ))
            })
    }

    /// Without the `uuid` feature there is no UUID to bind, so the function
    /// is refused rather than bound as text a `uuid` column would reject.
    #[cfg(not(feature = "uuid"))]
    fn uuid(_: &str) -> Result<Value, FilterClauseError> {
        Err(FilterClauseError::Unsupported(
            "`uuid` needs the `uuid` feature".to_owned(),
        ))
    }

    fn render_in_list(
        call: &CallExpr,
        values: &mut Vec<Value>,
    ) -> Result<String, FilterClauseError> {
        let [needle, haystack] = Self::pair(call)?;

        let Expr::List(list) = &haystack.expr else {
            return Err(FilterClauseError::Unsupported(
                "`in` reads a list on its right".to_owned(),
            ));
        };

        let needle_sql = Self::render(needle, values)?;
        let mut items = Vec::with_capacity(list.elements.len());
        for elem in &list.elements {
            items.push(Self::render(elem, values)?);
        }

        Ok(format!("{needle_sql} IN ({})", items.join(", ")))
    }

    /// A CEL literal, as the [`Value`] it means.
    fn literal(value: &LiteralValue) -> Result<Value, FilterClauseError> {
        Ok(match value {
            LiteralValue::String(string) => Value::String(string.to_string()),
            LiteralValue::Boolean(boolean) => Value::Bool(**boolean),
            LiteralValue::Int(int) => Value::Int(**int),
            // Refused rather than wrapped: a `u64` above `i64::MAX` has no
            // `Value::Int` to be, and silently binding it as a negative
            // number would compare against the wrong rows.
            LiteralValue::UInt(uint) => match i64::try_from(**uint) {
                Ok(int) => Value::Int(int),
                Err(_) => {
                    return Err(FilterClauseError::Unsupported(format!(
                        "unsigned literal {} is too large for the signed integer a bind value carries",
                        **uint
                    )));
                }
            },
            LiteralValue::Double(double) => Value::Float(**double),
            LiteralValue::Null => Value::Null,
            LiteralValue::Bytes(_) => {
                return Err(FilterClauseError::Unsupported(
                    "a bytes literal has no SQL spelling".to_owned(),
                ));
            }
        })
    }

    fn pair(call: &CallExpr) -> Result<[&cel::IdedExpr; 2], FilterClauseError> {
        match call.args.as_slice() {
            [left, right] => Ok([left, right]),
            _ => Err(FilterClauseError::Unsupported(format!(
                "`{}` reads two operands",
                call.func_name
            ))),
        }
    }

    fn only(call: &CallExpr) -> Result<&cel::IdedExpr, FilterClauseError> {
        match call.args.as_slice() {
            [only] => Ok(only),
            _ => Err(FilterClauseError::Unsupported(format!(
                "`{}` reads one operand",
                call.func_name
            ))),
        }
    }

    fn refused(node: &cel::IdedExpr) -> FilterClauseError {
        FilterClauseError::Unsupported(format!("{:?} has no reading as a condition", node.expr))
    }
}

// A separate inherent `impl FilterClause` block, rather than folding
// `rename`/`field_name` into the block above, purely to keep them
// textually next to the `QueryResolver` impl they exist for — Rust
// doesn't allow a non-trait helper to live inside
// `impl QueryResolver for FilterClause` itself, since a trait impl block
// may only contain that trait's own members.
impl FilterClause {
    /// The dotted field a CEL identifier or selection names --
    /// `v.created_at`.
    fn field_name(node: &cel::IdedExpr) -> Option<String> {
        match &node.expr {
            Expr::Ident(name) => Some(name.clone()),
            // `a.b?.c` tests for presence rather than naming a field.
            Expr::Select(select) if !select.test => Some(format!(
                "{}.{}",
                Self::field_name(&select.operand)?,
                select.field
            )),
            _ => None,
        }
    }

    /// Replaces every field reference in `node` with a flat `Ident` node
    /// carrying the column it resolves to — collapsing a dotted selection
    /// (`v.created_at`) into whatever text the allow-list names for it
    /// (which may itself be dotted, e.g. `v.rank_score`), so rendering
    /// never needs to know the difference between a bare and a qualified
    /// column.
    fn rename(
        node: &mut cel::IdedExpr,
        columns: &HashMap<&str, &str>,
    ) -> Result<(), FilterClauseError> {
        if let Some(name) = Self::field_name(node) {
            let column = columns
                .get(name.as_str())
                .ok_or_else(|| FilterClauseError::UnknownField(name.clone()))?;
            node.expr = Expr::Ident((*column).to_owned());
            return Ok(());
        }

        match &mut node.expr {
            Expr::Call(call) => {
                // A method's receiver is a field too -- `name.startsWith(..)`
                // -- and has to clear the allow-list like any other.
                if let Some(target) = call.target.as_deref_mut() {
                    Self::rename(target, columns)?;
                }
                for arg in &mut call.args {
                    Self::rename(arg, columns)?;
                }
            }
            Expr::List(list) => {
                for elem in &mut list.elements {
                    Self::rename(elem, columns)?;
                }
            }
            _ => {}
        }
        Ok(())
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
        if let Some(expr) = &mut self.expr {
            Self::rename(expr, columns)?;
        }
        Ok(self)
    }
}

impl From<FilterClause> for WhereClause {
    fn from(filter: FilterClause) -> Self {
        filter.to_where_clause()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed(filter: &str) -> String {
        FilterClause::parse(filter)
            .expect("condition parses")
            .to_where_clause()
            .sql()
            .as_str()
            .to_owned()
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
        let where_by = FilterClause::parse("status in ['live', 'draft']")
            .unwrap()
            .to_where_clause();

        assert_eq!(where_by.sql().as_str(), "status IN ($1, $2)");
        assert_eq!(
            where_by.values(),
            &[Value::String("live".into()), Value::String("draft".into())]
        );
    }

    #[test]
    fn not_keeps_what_it_applies_to_together() {
        assert_eq!(
            parsed("!(a == 1 && b == 2)"),
            "NOT (((a) = ($1)) AND ((b) = ($2)))"
        );
    }

    /// A blank filter is no condition: it resolves against any mapping, the
    /// empty one included, and renders as the empty clause.
    #[test]
    fn a_blank_filter_is_the_empty_one() {
        for blank in ["", "   ", "\n\t"] {
            let filter = FilterClause::parse(blank)
                .and_then(|filter| filter.resolve(&HashMap::new()))
                .expect("a blank filter parses and resolves");

            assert!(filter.to_where_clause().is_empty(), "{blank:?}");
        }
        assert!(FilterClause::default().to_where_clause().is_empty());
    }

    #[test]
    fn what_has_no_reading_as_a_condition_is_refused() {
        assert!(refuses("size(name) > 3"));
        assert!(refuses("name.size() > 3"));
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

        assert_eq!(
            filter.to_where_clause().sql().as_str(),
            "((state) = ($1)) AND ((v.rank_score) > ($2))"
        );
    }

    #[test]
    fn resolve_refuses_a_condition_naming_an_unoffered_field() {
        let refused = FilterClause::parse("status == 'live' && secret == 1")
            .expect("condition parses")
            .resolve(&columns(&[("status", "state")]));

        assert!(matches!(
            refused,
            Err(FilterClauseError::UnknownField(field)) if field == "secret"
        ));
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

    #[test]
    fn string_methods_read_as_like() {
        let pattern = |filter: &str| {
            let where_by = FilterClause::parse(filter)
                .expect("condition parses")
                .to_where_clause();
            (
                where_by.sql().as_str().to_owned(),
                where_by.values().to_vec(),
            )
        };

        assert_eq!(
            pattern("name.startsWith('Gro')"),
            (
                "(name) LIKE $1 ESCAPE '!'".to_owned(),
                vec![Value::String("Gro%".into())]
            )
        );
        assert_eq!(
            pattern("name.endsWith('ies')").1,
            vec![Value::String("%ies".into())]
        );
        assert_eq!(
            pattern("name.contains('cer')").1,
            vec![Value::String("%cer%".into())]
        );
    }

    #[test]
    fn a_like_pattern_matches_its_wildcards_literally() {
        let where_by = FilterClause::parse("code.startsWith('50%_off!')")
            .expect("condition parses")
            .to_where_clause();

        assert_eq!(where_by.values(), &[Value::String("50!%!_off!!%".into())]);
    }

    #[test]
    fn a_string_method_combines_like_any_condition() {
        assert_eq!(
            parsed("name.startsWith('a') && rank > 3"),
            "((name) LIKE $1 ESCAPE '!') AND ((rank) > ($2))"
        );
    }

    #[test]
    fn a_string_method_is_only_called_on_a_field_with_a_literal() {
        assert!(refuses("'abc'.startsWith('a')"));
        assert!(refuses("name.startsWith(other)"));
        assert!(refuses("name.startsWith(3)"));
        assert!(refuses("name.startsWith('a', 'b')"));
        assert!(refuses("startsWith(name, 'a')"));
    }

    #[test]
    fn resolve_renames_the_field_a_method_is_called_on() {
        let filter = FilterClause::parse("title.startsWith('Gro')")
            .expect("condition parses")
            .resolve(&columns(&[("title", "display_name")]))
            .expect("the field is named");

        assert_eq!(
            filter.to_where_clause().sql().as_str(),
            "(display_name) LIKE $1 ESCAPE '!'"
        );
    }

    #[test]
    fn resolve_refuses_a_method_on_an_unoffered_field() {
        let refused = FilterClause::parse("secret.startsWith('a')")
            .expect("condition parses")
            .resolve(&columns(&[("title", "display_name")]));

        assert!(matches!(
            refused,
            Err(FilterClauseError::UnknownField(field)) if field == "secret"
        ));
    }

    /// 2026-01-01T00:00:00Z, in microseconds since the epoch.
    #[cfg(any(feature = "chrono", feature = "time"))]
    const NEW_YEAR: i64 = 1_767_225_600_000_000;

    #[cfg(any(feature = "chrono", feature = "time"))]
    #[test]
    fn timestamp_reads_as_a_timestamp_literal() {
        let where_by = FilterClause::parse("created_at > timestamp('2026-01-01T00:00:00Z')")
            .expect("condition parses")
            .to_where_clause();

        assert_eq!(where_by.sql().as_str(), "(created_at) > ($1)");
        assert_eq!(where_by.values(), &[Value::Timestamp(NEW_YEAR)]);
    }

    /// The offset is read, not dropped: the same instant in two zones binds
    /// the same value.
    #[cfg(any(feature = "chrono", feature = "time"))]
    #[test]
    fn a_timestamp_is_read_in_its_own_offset() {
        let where_by = FilterClause::parse("created_at == timestamp('2026-01-01T02:00:00+02:00')")
            .expect("condition parses")
            .to_where_clause();

        assert_eq!(where_by.values(), &[Value::Timestamp(NEW_YEAR)]);
    }

    #[cfg(any(feature = "chrono", feature = "time"))]
    #[test]
    fn a_malformed_timestamp_is_an_invalid_literal() {
        assert!(matches!(
            FilterClause::parse("created_at > timestamp('yesterday')"),
            Err(FilterClauseError::InvalidLiteral(_))
        ));
    }

    #[test]
    fn timestamp_reads_one_string_literal() {
        assert!(refuses("created_at > timestamp(other)"));
        assert!(refuses("created_at > timestamp(3)"));
        assert!(refuses("created_at > timestamp('a', 'b')"));
        assert!(refuses(
            "created_at > name.timestamp('2026-01-01T00:00:00Z')"
        ));
    }

    #[cfg(not(any(feature = "chrono", feature = "time")))]
    #[test]
    fn timestamp_is_refused_without_a_date_library() {
        assert!(refuses("created_at > timestamp('2026-01-01T00:00:00Z')"));
    }

    #[cfg(feature = "uuid")]
    #[test]
    fn uuid_reads_as_a_uuid_literal() {
        let where_by = FilterClause::parse("id == uuid('01234567-89ab-cdef-0123-456789abcdef')")
            .expect("condition parses")
            .to_where_clause();

        assert_eq!(where_by.sql().as_str(), "(id) = ($1)");
        assert_eq!(
            where_by.values(),
            &[Value::Uuid([
                0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef, 0x01, 0x23, 0x45, 0x67, 0x89, 0xab,
                0xcd, 0xef,
            ])]
        );
    }

    #[cfg(feature = "uuid")]
    #[test]
    fn a_malformed_uuid_is_an_invalid_literal() {
        assert!(matches!(
            FilterClause::parse("id == uuid('not-a-uuid')"),
            Err(FilterClauseError::InvalidLiteral(_))
        ));
    }

    #[test]
    fn uuid_reads_one_string_literal() {
        assert!(refuses("id == uuid(other)"));
        assert!(refuses("id == uuid(3)"));
        assert!(refuses(
            "id == name.uuid('01234567-89ab-cdef-0123-456789abcdef')"
        ));
    }

    #[cfg(not(feature = "uuid"))]
    #[test]
    fn uuid_is_refused_without_the_feature() {
        assert!(refuses(
            "id == uuid('01234567-89ab-cdef-0123-456789abcdef')"
        ));
    }
}
