use std::collections::HashMap;
use std::fmt;

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
pub struct OrderBy {
    terms: Vec<Term>,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum OrderByError {
    #[error("empty order_by term in `{0}`")]
    EmptyTerm(String),
    #[error("invalid sort direction `{direction}` for field `{field}`")]
    InvalidDirection { field: String, direction: String },
    #[error("unknown order_by field `{0}`")]
    UnknownField(String),
}

impl OrderBy {
    /// Parses `"field [asc|desc], field2 [asc|desc], ..."`. An empty or
    /// all-whitespace string parses to an empty `OrderBy`, which renders
    /// to an empty fragment (and is dropped by the composer's sentinel,
    /// same as an unset `order_by`).
    pub fn parse(order_by: &str) -> Result<Self, OrderByError> {
        let order_by = order_by.trim();
        if order_by.is_empty() {
            return Ok(OrderBy::default());
        }

        let mut terms = Vec::new();
        for part in order_by.split(',') {
            let part = part.trim();
            if part.is_empty() {
                return Err(OrderByError::EmptyTerm(order_by.to_owned()));
            }

            let mut words = part.split_whitespace();
            let field = words.next().unwrap_or_default().to_owned();
            if field.is_empty() {
                return Err(OrderByError::EmptyTerm(order_by.to_owned()));
            }

            let direction = match words.next() {
                None => Direction::Asc,
                Some(word) if word.eq_ignore_ascii_case("asc") => Direction::Asc,
                Some(word) if word.eq_ignore_ascii_case("desc") => Direction::Desc,
                Some(word) => {
                    return Err(OrderByError::InvalidDirection {
                        field,
                        direction: word.to_owned(),
                    });
                }
            };

            if words.next().is_some() {
                return Err(OrderByError::EmptyTerm(order_by.to_owned()));
            }

            terms.push(Term { field, direction });
        }

        Ok(OrderBy { terms })
    }
}

impl QueryResolver for OrderBy {
    type Error = OrderByError;

    fn resolve(mut self, columns: &HashMap<&str, &str>) -> Result<Self, Self::Error> {
        for term in &mut self.terms {
            match columns.get(term.field.as_str()) {
                Some(column) => term.field = (*column).to_owned(),
                None => return Err(OrderByError::UnknownField(term.field.clone())),
            }
        }
        Ok(self)
    }
}

impl OrderBy {
    /// Renders to `"col1 ASC, col2 DESC"`. `OrderBy` never carries bind
    /// values (it only ever emits column names and directions), so unlike
    /// [`Where`](crate::Where) its sentinel never needs placeholder
    /// shifting — see [`QueryComposer::order_by`](crate::QueryComposer::order_by).
    pub(crate) fn render(&self) -> String {
        self.terms
            .iter()
            .map(|term| format!("{} {}", term.field, term.direction))
            .collect::<Vec<_>>()
            .join(", ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_default_direction() {
        let order_by = OrderBy::parse("rank").unwrap();
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
        let order_by = OrderBy::parse("rank DESC, created asc").unwrap();
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
        let order_by = OrderBy::parse("").unwrap();
        assert_eq!(order_by, OrderBy::default());
        assert_eq!(order_by.render(), "");
    }

    #[test]
    fn rejects_invalid_direction() {
        let err = OrderBy::parse("rank sideways").unwrap_err();
        assert_eq!(
            err,
            OrderByError::InvalidDirection {
                field: "rank".into(),
                direction: "sideways".into()
            }
        );
    }

    #[test]
    fn rejects_empty_term() {
        let err = OrderBy::parse("rank,,created").unwrap_err();
        assert_eq!(err, OrderByError::EmptyTerm("rank,,created".into()));
    }

    #[test]
    fn resolve_renames_fields_against_allow_list() {
        let columns = HashMap::from([("rank", "rank"), ("created", "created_at")]);
        let order_by = OrderBy::parse("created desc, rank")
            .unwrap()
            .resolve(&columns)
            .unwrap();
        assert_eq!(order_by.render(), "created_at DESC, rank ASC");
    }

    #[test]
    fn resolve_fails_closed_on_unknown_field() {
        let columns = HashMap::from([("rank", "rank")]);
        let err = OrderBy::parse("internal_notes")
            .unwrap()
            .resolve(&columns)
            .unwrap_err();
        assert_eq!(err, OrderByError::UnknownField("internal_notes".into()));
    }
}
