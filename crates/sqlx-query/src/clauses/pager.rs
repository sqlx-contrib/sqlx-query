//! Keyset ("seek method") pagination cursor: an OR-of-ANDs tuple
//! comparison built from a resolved [`OrderByClause`] and one boundary
//! value per key.
//!
//! `Cursor::parse` (decoding an opaque `page_token` from the client) and
//! `Cursor::after_row` (filling values straight from a `sqlx::Row`) are
//! deliberately not implemented yet — both depend on design decisions
//! this crate hasn't settled: `parse` needs a token serialization/
//! checksum format, and `after_row` needs a per-`QueryDialect` table
//! mapping column types to [`Value`] variants (decoding is the opposite
//! direction from `QueryComposer::build`'s `Encode`/`Type` bounds: there,
//! the source Rust type is always known; here, a column alone doesn't say
//! which `Value` variant it should become).

use super::order::OrderKey;
use crate::{OrderByClause, OrderDirection, Value, WhereClause};

/// One ordered comparison key: a resolved column/direction pair, paired
/// with the cursor's boundary value for it. `value` is `None` between
/// [`Cursor::new`] and [`Cursor::after`] — never observable from outside
/// this module, since `Cursor`'s only public accessors
/// ([`Cursor::to_order_by_clause`], [`Cursor::to_where_clause`]) assume
/// every key already has one.
#[derive(Debug)]
struct CursorKey {
    key: OrderKey,
    value: Option<Value>,
}

/// A keyset pagination cursor. See the module docs for what's built and
/// what's deliberately not here yet.
#[derive(Debug)]
pub struct Cursor {
    keys: Vec<CursorKey>,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum CursorError {
    #[error("expected {expected} cursor values (one per order_by key), got {actual}")]
    ValueCountMismatch { expected: usize, actual: usize },
}

impl Cursor {
    /// Clause only, no boundary values yet — supply them with
    /// [`after`](Self::after).
    pub fn new(order_by: impl Into<OrderByClause>) -> Self {
        let keys = order_by
            .into()
            .keys()
            .iter()
            .cloned()
            .map(|key| CursorKey { key, value: None })
            .collect();
        Cursor { keys }
    }

    /// Supplies all boundary values at once, one per key, in order — not
    /// chainable per-value like [`WhereClause::bind`], since a cursor
    /// position isn't meaningful with only some of its values known.
    pub fn after(mut self, values: Vec<Value>) -> Result<Self, CursorError> {
        if values.len() != self.keys.len() {
            return Err(CursorError::ValueCountMismatch {
                expected: self.keys.len(),
                actual: values.len(),
            });
        }
        for (key, value) in self.keys.iter_mut().zip(values) {
            key.value = Some(value);
        }
        Ok(self)
    }

    /// Reconstructs the `OrderByClause` this cursor was built against, to
    /// re-apply to the next page's query — see
    /// [`QueryComposer::with_cursor`](crate::QueryComposer::with_cursor).
    pub fn to_order_by_clause(&self) -> OrderByClause {
        self.keys.iter().map(|ck| ck.key.clone()).collect()
    }

    /// The tuple-comparison `WHERE` fragment: for keys `k1..kn` with
    /// directions `d1..dn` and boundary values `v1..vn`,
    /// `(k1 OP1 v1) OR (k1 = v1 AND k2 OP2 v2) OR ...`, where `OPi` is `>`
    /// for an ascending key and `<` for a descending one.
    ///
    /// Panics if any key's value is unset — unreachable through this
    /// type's public API, since only [`after`](Self::after) sets values,
    /// and it always sets all of them together.
    pub fn to_where_clause(&self) -> WhereClause {
        let mut branches = Vec::with_capacity(self.keys.len());
        for i in 0..self.keys.len() {
            let mut conditions = Vec::with_capacity(i + 1);
            for prior in &self.keys[..i] {
                conditions.push(format!(
                    "{} = ${}",
                    prior.key.column(),
                    placeholder_index(&self.keys, prior)
                ));
            }
            let key = &self.keys[i];
            let op = match key.key.direction() {
                OrderDirection::Asc => '>',
                OrderDirection::Desc => '<',
            };
            conditions.push(format!(
                "{} {op} ${}",
                key.key.column(),
                placeholder_index(&self.keys, key)
            ));
            branches.push(format!("({})", conditions.join(" AND ")));
        }

        let sql = branches.join(" OR ");
        self.keys
            .iter()
            .fold(WhereClause::new(sql), |where_by, key| {
                where_by.bind(
                    key.value
                        .clone()
                        .expect("Cursor::to_where_clause called before after() set all values"),
                )
            })
    }
}

/// `key`'s 1-based position in `keys` — its placeholder number, since
/// each key's value is bound once and referenced by number wherever it
/// recurs across the OR branches.
fn placeholder_index(keys: &[CursorKey], key: &CursorKey) -> usize {
    keys.iter().position(|k| std::ptr::eq(k, key)).unwrap() + 1
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_tuple_comparison_for_two_keys() {
        let order_by = OrderByClause::parse("rank desc, id asc").unwrap();
        let cursor = Cursor::new(order_by)
            .after(vec![Value::Int(42), Value::Int(7)])
            .unwrap();

        let where_by = cursor.to_where_clause();

        assert_eq!(
            where_by.sql().as_str(),
            "(rank < $1) OR (rank = $1 AND id > $2)"
        );
        assert_eq!(where_by.values(), &[Value::Int(42), Value::Int(7)]);
    }

    #[test]
    fn after_rejects_wrong_value_count() {
        let order_by = OrderByClause::parse("rank desc, id asc").unwrap();
        let err = Cursor::new(order_by)
            .after(vec![Value::Int(1)])
            .unwrap_err();
        assert_eq!(
            err,
            CursorError::ValueCountMismatch {
                expected: 2,
                actual: 1
            }
        );
    }

    #[test]
    fn order_by_round_trips() {
        let order_by = OrderByClause::parse("rank desc, id asc").unwrap();
        let cursor = Cursor::new(order_by.clone())
            .after(vec![Value::Int(42), Value::Int(7)])
            .unwrap();

        assert_eq!(cursor.to_order_by_clause(), order_by);
    }
}
