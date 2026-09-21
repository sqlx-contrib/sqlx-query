use std::collections::HashMap;
use std::fmt;

use serde::{Deserialize, Serialize};
use sqlx::{AssertSqlSafe, SqlSafeStr, SqlStr};

use crate::QueryResolver;

/// One `ORDER BY` key's sort direction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum OrderDirection {
    Asc,
    Desc,
}

impl fmt::Display for OrderDirection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            OrderDirection::Asc => f.write_str("ASC"),
            OrderDirection::Desc => f.write_str("DESC"),
        }
    }
}

/// A single resolved sort key: column + direction. `pub(crate)` (not
/// `pub`) so `Cursor` can reuse it (it needs the same column/direction
/// pairs to build its tuple comparison) without exposing a mutable way
/// to bypass `resolve()`'s allow-list from outside this crate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct OrderKey {
    column: String,
    direction: OrderDirection,
}

impl OrderKey {
    pub(crate) fn column(&self) -> &str {
        &self.column
    }

    pub(crate) fn direction(&self) -> OrderDirection {
        self.direction
    }
}

/// A parsed AIP-132 `order_by` value: `"field [asc|desc], ..."`. No CEL
/// involved — this is a plain comma-separated field list.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OrderByClause {
    keys: Vec<OrderKey>,
}

impl FromIterator<OrderKey> for OrderByClause {
    fn from_iter<T: IntoIterator<Item = OrderKey>>(iter: T) -> Self {
        OrderByClause {
            keys: iter.into_iter().collect(),
        }
    }
}

/// Errors [`OrderByClause::parse`]/[`OrderByClause::resolve`] can return.
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

        let mut keys = Vec::new();
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
                None => OrderDirection::Asc,
                Some(word) if word.eq_ignore_ascii_case("asc") => OrderDirection::Asc,
                Some(word) if word.eq_ignore_ascii_case("desc") => OrderDirection::Desc,
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

            keys.push(OrderKey {
                column: field,
                direction,
            });
        }

        Ok(OrderByClause { keys })
    }

    /// Renders to `"col1 ASC, col2 DESC"`, returning the same [`SqlStr`]
    /// type as [`WhereClause::sql`](crate::WhereClause::sql) so both clause
    /// types answer "what's your SQL text?" identically. Unlike
    /// `WhereClause`'s (a cheap clone of an `Arc`-backed field), this is
    /// computed fresh from `keys` on every call — `OrderByClause` never
    /// carries bind values, so there's no matching `values()`; see
    /// [`QueryComposer::push_order_by`](crate::QueryComposer::push_order_by).
    pub fn sql(&self) -> SqlStr {
        let sql = self
            .keys
            .iter()
            .map(|key| format!("{} {}", key.column, key.direction))
            .collect::<Vec<_>>()
            .join(", ");
        AssertSqlSafe(sql).into_sql_str()
    }

    /// The resolved column/direction pairs, in order — `pub(crate)` for
    /// `Cursor` to build its tuple comparison from.
    pub(crate) fn keys(&self) -> &[OrderKey] {
        &self.keys
    }

    /// Appends `other`'s keys after this clause's own, as tie-breakers —
    /// "sort by `self`, **then** by `other`". Unlike
    /// [`WhereClause::and`](crate::WhereClause::and), this isn't
    /// commutative: `a.then(b)` sorts by `a` first, `b.then(a)` sorts by
    /// `b` first — order matters, because ORDER BY is a sequence, not a
    /// boolean combination like WHERE's `AND`.
    pub fn then(mut self, other: OrderByClause) -> OrderByClause {
        self.keys.extend(other.keys);
        self
    }
}

impl QueryResolver for OrderByClause {
    type Error = OrderByClauseError;

    /// Renames every field this ordering names to the column it stands
    /// for, against a fail-closed allow-list: a field the map doesn't
    /// have is refused, so a request can't sort on a column it wasn't
    /// offered.
    fn resolve(mut self, columns: &HashMap<&str, &str>) -> Result<Self, Self::Error> {
        for key in &mut self.keys {
            match columns.get(key.column.as_str()) {
                Some(column) => key.column = (*column).to_owned(),
                None => return Err(OrderByClauseError::UnknownField(key.column.clone())),
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
            order_by.keys,
            vec![OrderKey {
                column: "rank".into(),
                direction: OrderDirection::Asc
            }]
        );
    }

    #[test]
    fn parses_explicit_direction_case_insensitively() {
        let order_by = OrderByClause::parse("rank DESC, created asc").unwrap();
        assert_eq!(
            order_by.keys,
            vec![
                OrderKey {
                    column: "rank".into(),
                    direction: OrderDirection::Desc
                },
                OrderKey {
                    column: "created".into(),
                    direction: OrderDirection::Asc
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

    #[test]
    fn then_appends_as_a_tie_breaker_not_commutatively() {
        let tenant_first = OrderByClause::parse("tenant_id asc")
            .unwrap()
            .then(OrderByClause::parse("rank desc").unwrap());
        assert_eq!(tenant_first.sql(), "tenant_id ASC, rank DESC");

        let rank_first = OrderByClause::parse("rank desc")
            .unwrap()
            .then(OrderByClause::parse("tenant_id asc").unwrap());
        assert_eq!(rank_first.sql(), "rank DESC, tenant_id ASC");
    }
}
