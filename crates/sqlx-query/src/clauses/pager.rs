//! Keyset ("seek method") pagination cursor: an OR-of-ANDs tuple
//! comparison built from a resolved [`OrderByClause`] and one boundary
//! value per key.
//!
//! [`Cursor::encode`]/[`Cursor::parse`] round-trip a cursor through an
//! opaque `page_token` string, following `einride/aip-go`'s
//! `pagination.PageToken` shape: a small envelope struct carrying a
//! `checksum` field alongside the payload, `postcard`-serialized and
//! base64-wrapped. There's no separate version field — instead, like
//! `einride/aip-go`'s `pageTokenChecksumMask`, the checksum is XORed with
//! [`CURSOR_TOKEN_CHECKSUM_MASK`] on the way out and in; bump that
//! constant whenever the payload shape changes in a way that would make
//! an already-issued token decode to something different, and every old
//! token's checksum will mismatch and be cleanly rejected rather than
//! misread.
//!
//! That checksum only guards against a corrupted or hand-edited token; it
//! is not a substitute for
//! [`QueryComposer::compose`](crate::QueryComposer::compose)'s
//! `CursorOrderByMismatch` check, which is what catches a client paging
//! with one `order_by` and then switching to another — that check compares
//! the actual decoded `OrderByClause`, a stronger guarantee than a hash
//! could give.
//!
//! [`Cursor::after_row`] fills values straight from a `sqlx::Row` (the
//! last row of a page, once it's been fetched) instead of requiring the
//! caller to already know each key's Rust type. It doesn't need a
//! per-`QueryDialect` table mapping column types to [`Value`] variants —
//! `sqlx` already has that knowledge, per-driver, baked into each
//! `Type::compatible` check, so it just *asks* by trying candidate
//! decodes in turn and keeping whichever one `sqlx` itself accepts,
//! rather than this crate re-deriving the same table from OIDs or type
//! codes.

use serde::{Deserialize, Serialize};
use sqlx::{ColumnIndex, Decode, Row, Type};

use super::order::OrderKey;
use crate::value::RowExtension;
use crate::{OrderByClause, OrderDirection, Value, WhereClause};

/// [`Cursor::encode`]'s envelope — a `checksum` field alongside the
/// payload, mirroring `einride/aip-go`'s `PageToken { Offset, checksum
/// RequestChecksum }`. Private: callers only ever see the base64 string
/// [`Cursor::encode`] returns, never this type.
#[derive(Serialize, Deserialize)]
struct CursorToken {
    checksum: u32,
    cursor: Cursor,
}

/// One ordered comparison key: a resolved column/direction pair, paired
/// with the cursor's boundary value for it. `value` is `None` between
/// [`Cursor::new`] and [`Cursor::after`] — never observable from outside
/// this module, since `Cursor`'s only public accessors
/// ([`Cursor::to_order_by_clause`], [`Cursor::to_where_clause`],
/// [`Cursor::encode`]) assume every key already has one.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct CursorKey {
    order: OrderKey,
    value: Option<Value>,
}

/// A keyset pagination cursor. See the module docs for what's built and
/// what's deliberately not here yet.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Cursor {
    keys: Vec<CursorKey>,
}

/// Errors [`Cursor`]'s methods can return: building one
/// ([`Cursor::after`]/[`Cursor::after_row`]), decoding a token
/// ([`Cursor::parse`]), or reading a bind value off a row.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum CursorError {
    #[error("expected {expected} cursor values (one per order_by key), got {actual}")]
    ValueCountMismatch { expected: usize, actual: usize },

    #[error("cursor token is not valid base64")]
    TokenInvalidBase64,

    #[error("malformed cursor token")]
    TokenMalformed,

    #[error("cursor token checksum mismatch — the token was corrupted or tampered with")]
    TokenChecksumMismatch,

    #[error("row has no column named `{0}` for an order_by key")]
    RowColumnMissing(String),

    #[error(
        "column `{0}` is not one of the types a cursor can carry (bool, int, float, string, timestamp)"
    )]
    RowValueUndecodable(String),
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
            .map(|order| CursorKey { order, value: None })
            .collect();
        Cursor { keys }
    }

    /// Decodes a `page_token` string produced by [`encode`](Self::encode)
    /// back into a `Cursor` carrying both its `order_by` and its boundary
    /// values — no separate [`new`](Self::new)/[`after`](Self::after) call
    /// needed, unlike building one from scratch.
    ///
    /// # Errors
    ///
    /// [`CursorError::TokenInvalidBase64`] or
    /// [`CursorError::TokenMalformed`] for a token that isn't one this type
    /// produced, and [`CursorError::TokenChecksumMismatch`] for one that was
    /// corrupted or hand-edited after it was issued.
    pub fn parse(token: &str) -> Result<Self, CursorError> {
        use base64::Engine as _;

        let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(token)
            .map_err(|_| CursorError::TokenInvalidBase64)?;

        let wire: CursorToken =
            postcard::from_bytes(&bytes).map_err(|_| CursorError::TokenMalformed)?;

        if checksum_of(&wire.cursor) != wire.checksum {
            return Err(CursorError::TokenChecksumMismatch);
        }

        Ok(wire.cursor)
    }

    /// Encodes this cursor as an opaque `page_token` string, safe to hand
    /// back to the client as-is. See the module docs for the wire format
    /// and what the checksum does and doesn't guard against.
    ///
    /// # Panics
    ///
    /// If any key's value is unset — unreachable through this type's public
    /// API, same as [`to_where_clause`](Self::to_where_clause).
    #[must_use]
    pub fn encode(&self) -> String {
        use base64::Engine as _;

        let token = CursorToken {
            checksum: checksum_of(self),
            cursor: self.clone(),
        };
        let bytes = postcard::to_allocvec(&token).expect("CursorToken serialization is infallible");

        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
    }

    /// Supplies all boundary values at once, one per key, in order — not
    /// chainable per-value like [`WhereClause::bind_value`], since a cursor
    /// position isn't meaningful with only some of its values known.
    ///
    /// # Errors
    ///
    /// [`CursorError::ValueCountMismatch`] unless `values` has exactly one
    /// entry per `order_by` key.
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

    /// Supplies all boundary values at once by reading them off `row` —
    /// the last row of a page just fetched, once decoded, becomes the
    /// cursor for the next one. Each key's column is looked up by name
    /// (a table-qualified `order_by` column resolves against the row's
    /// own, always-unqualified, label — e.g. `v.created_at` matches a
    /// row column named `created_at`), so `row`'s column order doesn't
    /// need to match `self`'s key order.
    ///
    /// # Errors
    ///
    /// [`CursorError::RowColumnMissing`] for a key whose column `row`
    /// doesn't carry, and [`CursorError::RowValueUndecodable`] for one whose
    /// column isn't any of the types a [`Value`] can hold.
    pub fn after_row<'r, R>(mut self, row: &'r R) -> Result<Self, CursorError>
    where
        R: Row,
        usize: ColumnIndex<R>,
        bool: Decode<'r, R::Database> + Type<R::Database>,
        i16: Decode<'r, R::Database> + Type<R::Database>,
        i32: Decode<'r, R::Database> + Type<R::Database>,
        i64: Decode<'r, R::Database> + Type<R::Database>,
        f32: Decode<'r, R::Database> + Type<R::Database>,
        f64: Decode<'r, R::Database> + Type<R::Database>,
        String: Decode<'r, R::Database> + Type<R::Database>,
        crate::value::TimestampRepr: Decode<'r, R::Database> + Type<R::Database>,
        Vec<u8>: Decode<'r, R::Database> + Type<R::Database>,
    {
        for key in &mut self.keys {
            key.value = Some(row.get_value(key.order.column())?);
        }
        Ok(self)
    }

    /// Reconstructs the `OrderByClause` this cursor was built against, to
    /// re-apply to the next page's query — see
    /// [`QueryComposer::with_cursor`](crate::QueryComposer::with_cursor).
    #[must_use]
    pub fn to_order_by_clause(&self) -> OrderByClause {
        self.keys.iter().map(|ck| ck.order.clone()).collect()
    }

    /// The tuple-comparison `WHERE` fragment: for keys `k1..kn` with
    /// directions `d1..dn` and boundary values `v1..vn`,
    /// `(k1 OP1 v1) OR (k1 = v1 AND k2 OP2 v2) OR ...`, where `OPi` is `>`
    /// for an ascending key and `<` for a descending one.
    ///
    /// # Panics
    ///
    /// If any key's value is unset — unreachable through this type's public
    /// API, since only [`after`](Self::after) and [`parse`](Self::parse) set
    /// values, and both always set all of them together.
    #[must_use]
    pub fn to_where_clause(&self) -> WhereClause {
        let mut branches = Vec::with_capacity(self.keys.len());
        for i in 0..self.keys.len() {
            let mut conditions = Vec::with_capacity(i + 1);
            for prior in &self.keys[..i] {
                conditions.push(format!(
                    "{} = ${}",
                    prior.order.column(),
                    placeholder_index(&self.keys, prior)
                ));
            }
            let key = &self.keys[i];
            let op = match key.order.direction() {
                OrderDirection::Asc => '>',
                OrderDirection::Desc => '<',
            };
            conditions.push(format!(
                "{} {op} ${}",
                key.order.column(),
                placeholder_index(&self.keys, key)
            ));
            branches.push(format!("({})", conditions.join(" AND ")));
        }

        let sql = branches.join(" OR ");
        self.keys
            .iter()
            .fold(WhereClause::new(sql), |where_by, key| {
                where_by.bind_value(
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

/// XORed into the checksum on both [`Cursor::encode`] and
/// [`Cursor::parse`] — see the module docs for what bumping this buys.
const CURSOR_TOKEN_CHECKSUM_MASK: u32 = 0x5eed_c0de;

/// `cursor`'s checksum, as stored in / verified against a
/// [`CursorToken`]: a CRC32 of `cursor`'s own `postcard` encoding, masked
/// by [`CURSOR_TOKEN_CHECKSUM_MASK`]. Deterministic — `postcard`'s output
/// for the same value is always the same bytes — so [`Cursor::parse`] can
/// recompute this straight from the decoded `cursor` field rather than
/// needing the original payload bytes kept around separately.
fn checksum_of(cursor: &Cursor) -> u32 {
    let payload = postcard::to_allocvec(cursor).expect("Cursor serialization is infallible");
    crc32(&payload) ^ CURSOR_TOKEN_CHECKSUM_MASK
}

/// IEEE CRC32 (the same variant Go's `hash/crc32.ChecksumIEEE` computes),
/// hand-rolled bitwise rather than table-driven — this only ever runs once
/// per `encode`/`parse` call over a few dozen bytes, so the table's
/// speedup isn't worth the extra static data. Verified against the
/// standard `"123456789"` -> `0xCBF43926` test vector below.
fn crc32(bytes: &[u8]) -> u32 {
    const POLY: u32 = 0xEDB8_8320;
    let mut crc = 0xFFFF_FFFFu32;
    for &byte in bytes {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (POLY & mask);
        }
    }
    !crc
}

#[cfg(test)]
mod tests {
    use base64::Engine as _;

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

    #[test]
    fn crc32_matches_known_test_vector() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    }

    #[test]
    fn encode_parse_round_trips_order_by_and_values() {
        let order_by = OrderByClause::parse("rank desc, name asc").unwrap();
        let cursor = Cursor::new(order_by.clone())
            .after(vec![Value::Int(42), Value::String("alice".into())])
            .unwrap();

        let parsed = Cursor::parse(&cursor.encode()).unwrap();

        assert_eq!(parsed.to_order_by_clause(), order_by);
        assert_eq!(parsed.to_where_clause(), cursor.to_where_clause());
    }

    #[test]
    fn encode_parse_round_trips_every_value_variant() {
        let order_by = OrderByClause::parse("a asc, b asc, c asc, d asc, e asc, f asc").unwrap();
        let timestamp = 1_700_000_000_123_000i64;
        let cursor = Cursor::new(order_by)
            .after(vec![
                Value::Null,
                Value::Bool(true),
                Value::Int(-7),
                Value::Float(3.5),
                Value::String("hello".into()),
                Value::Timestamp(timestamp),
            ])
            .unwrap();

        let parsed = Cursor::parse(&cursor.encode()).unwrap();

        assert_eq!(
            parsed.to_where_clause().values(),
            cursor.to_where_clause().values()
        );
    }

    #[test]
    fn parse_rejects_invalid_base64() {
        let err = Cursor::parse("not valid base64!!").unwrap_err();
        assert_eq!(err, CursorError::TokenInvalidBase64);
    }

    #[test]
    fn parse_rejects_garbage_bytes() {
        let token = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode([1u8, 2, 3]);
        let err = Cursor::parse(&token).unwrap_err();
        assert_eq!(err, CursorError::TokenMalformed);
    }

    #[test]
    fn parse_rejects_corrupted_token() {
        let order_by = OrderByClause::parse("rank desc").unwrap();
        let cursor = Cursor::new(order_by).after(vec![Value::Int(42)]).unwrap();
        let token = cursor.encode();

        let mut wire = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(&token)
            .unwrap();
        let last = wire.len() - 1;
        wire[last] ^= 0xFF;
        let tampered = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&wire);

        let err = Cursor::parse(&tampered).unwrap_err();
        assert!(matches!(
            err,
            CursorError::TokenChecksumMismatch | CursorError::TokenMalformed
        ));
    }

    #[test]
    fn checksum_mask_distinguishes_from_a_plain_crc32() {
        let order_by = OrderByClause::parse("rank desc").unwrap();
        let cursor = Cursor::new(order_by).after(vec![Value::Int(42)]).unwrap();

        let payload = postcard::to_allocvec(&cursor).unwrap();
        assert_ne!(checksum_of(&cursor), crc32(&payload));
    }
}
