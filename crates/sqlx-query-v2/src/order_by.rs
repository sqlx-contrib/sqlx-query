use std::collections::HashMap;
use std::fmt;

use sqlx::{AssertSqlSafe, SqlSafeStr, SqlStr};

use crate::QueryResolver;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Asc,
    Desc,
}

impl fmt::Display for Direction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Direction::Asc => f.write_str("ASC"),
            Direction::Desc => f.write_str("DESC"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Term {
    field: String,
    direction: Direction,
}

/// A parsed AIP-132 `order_by` value: `"field [asc|desc], ..."`. No CEL
/// involved — this is a plain comma-separated field list.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OrderByClause {
    terms: Vec<Term>,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum OrderByClauseError {
    #[error("empty order_by term in `{0}`")]
    EmptyTerm(String),
    #[error("invalid sort direction `{direction}` for field `{field}`")]
    InvalidDirection { field: String, direction: String },
    #[error("unknown order_by field `{0}`")]
    UnknownField(String),
}

impl OrderByClause {
    /// Parses `"field [asc|desc], field2 [asc|desc], ..."`. An empty or
    /// all-whitespace string parses to an empty `OrderByClause`, which renders
    /// to an empty fragment (and is dropped by the composer's sentinel,
    /// same as an unset `order_by`).
    pub fn parse(order_by: &str) -> Result<Self, OrderByClauseError> {
        let order_by = order_by.trim();
        if order_by.is_empty() {
            return Ok(OrderByClause::default());
        }

        let mut terms = Vec::new();
        for part in order_by.split(',') {
            let part = part.trim();
            if part.is_empty() {
                return Err(OrderByClauseError::EmptyTerm(order_by.to_owned()));
            }

            let mut words = part.split_whitespace();
            let field = words.next().unwrap_or_default().to_owned();
            if field.is_empty() {
                return Err(OrderByClauseError::EmptyTerm(order_by.to_owned()));
            }

            let direction = match words.next() {
                None => Direction::Asc,
                Some(word) if word.eq_ignore_ascii_case("asc") => Direction::Asc,
                Some(word) if word.eq_ignore_ascii_case("desc") => Direction::Desc,
                Some(word) => {
                    return Err(OrderByClauseError::InvalidDirection {
                        field,
                        direction: word.to_owned(),
                    });
                }
            };

            if words.next().is_some() {
                return Err(OrderByClauseError::EmptyTerm(order_by.to_owned()));
            }

            terms.push(Term { field, direction });
        }

        Ok(OrderByClause { terms })
    }

    /// Renders to `"col1 ASC, col2 DESC"`, returning the same [`SqlStr`]
    /// type as [`WhereClause::sql`](crate::WhereClause::sql) so both clause
    /// types answer "what's your SQL text?" identically. Unlike
    /// `WhereClause`'s (a cheap clone of an `Arc`-backed field), this is
    /// computed fresh from `terms` on every call — `OrderByClause` never
    /// carries bind values, so there's no matching `values()`; see
    /// [`QueryComposer::order_by`](crate::QueryComposer::order_by).
    pub fn sql(&self) -> SqlStr {
        let sql = self
            .terms
            .iter()
            .map(|term| format!("{} {}", term.field, term.direction))
            .collect::<Vec<_>>()
            .join(", ");
        AssertSqlSafe(sql).into_sql_str()
    }
}

impl QueryResolver for OrderByClause {
    type Error = OrderByClauseError;

    fn resolve(mut self, columns: &HashMap<&str, &str>) -> Result<Self, Self::Error> {
        for term in &mut self.terms {
            match columns.get(term.field.as_str()) {
                Some(column) => term.field = (*column).to_owned(),
                None => return Err(OrderByClauseError::UnknownField(term.field.clone())),
            }
        }
        Ok(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_default_direction() {
        let order_by = OrderByClause::parse("rank").unwrap();
        assert_eq!(
            order_by.terms,
            vec![Term {
                field: "rank".into(),
                direction: Direction::Asc
            }]
        );
    }

    #[test]
    fn parses_explicit_direction_case_insensitively() {
        let order_by = OrderByClause::parse("rank DESC, created asc").unwrap();
        assert_eq!(
            order_by.terms,
            vec![
                Term {
                    field: "rank".into(),
                    direction: Direction::Desc
                },
                Term {
                    field: "created".into(),
                    direction: Direction::Asc
                },
            ]
        );
    }

    #[test]
    fn empty_string_parses_to_empty_order_by() {
        let order_by = OrderByClause::parse("").unwrap();
        assert_eq!(order_by, OrderByClause::default());
        assert_eq!(order_by.sql(), "");
    }

    #[test]
    fn rejects_invalid_direction() {
        let err = OrderByClause::parse("rank sideways").unwrap_err();
        assert_eq!(
            err,
            OrderByClauseError::InvalidDirection {
                field: "rank".into(),
                direction: "sideways".into()
            }
        );
    }

    #[test]
    fn rejects_empty_term() {
        let err = OrderByClause::parse("rank,,created").unwrap_err();
        assert_eq!(err, OrderByClauseError::EmptyTerm("rank,,created".into()));
    }

    #[test]
    fn resolve_renames_fields_against_allow_list() {
        let columns = HashMap::from([("rank", "rank"), ("created", "created_at")]);
        let order_by = OrderByClause::parse("created desc, rank")
            .unwrap()
            .resolve(&columns)
            .unwrap();
        assert_eq!(order_by.sql(), "created_at DESC, rank ASC");
    }

    #[test]
    fn resolve_fails_closed_on_unknown_field() {
        let columns = HashMap::from([("rank", "rank")]);
        let err = OrderByClause::parse("internal_notes")
            .unwrap()
            .resolve(&columns)
            .unwrap_err();
        assert_eq!(
            err,
            OrderByClauseError::UnknownField("internal_notes".into())
        );
    }
}
