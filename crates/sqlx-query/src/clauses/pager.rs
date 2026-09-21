//! Keyset ("seek method") pagination cursor: an OR-of-ANDs tuple
//! comparison built from a resolved [`OrderByClause`] and one boundary
//! value per key.
//!
//! [`Cursor::encode`]/[`Cursor::parse`] round-trip a cursor through an
//! opaque `page_token` string — a small hand-rolled binary format (version
//! byte, CRC32 checksum, then a column/direction/value triple per key),
//! modeled on `protoc-contrib/aip-go`'s `PageCursor` wire format. The
//! checksum here only guards against a corrupted or hand-edited token; it
//! is not a substitute for
//! [`QueryComposer::compose`](crate::QueryComposer::compose)'s
//! `CursorOrderByMismatch` check, which is what catches a client paging
//! with one `order_by` and then switching to another — that check compares
//! the actual decoded `OrderByClause`, a stronger guarantee than a hash
//! could give.
//!
//! `Cursor::after_row` (filling values straight from a `sqlx::Row`) is
//! deliberately not implemented yet — it needs a per-`QueryDialect` table
//! mapping column types to [`Value`] variants (decoding is the opposite
//! direction from `QueryComposer::build`'s `Encode`/`Type` bounds: there,
//! the source Rust type is always known; here, a column alone doesn't say
//! which `Value` variant it should become).

use base64::Engine as _;
use chrono::{DateTime, Utc};

use super::order::OrderKey;
use crate::{OrderByClause, OrderDirection, Value, WhereClause};

/// One ordered comparison key: a resolved column/direction pair, paired
/// with the cursor's boundary value for it. `value` is `None` between
/// [`Cursor::new`] and [`Cursor::after`] — never observable from outside
/// this module, since `Cursor`'s only public accessors
/// ([`Cursor::to_order_by_clause`], [`Cursor::to_where_clause`],
/// [`Cursor::encode`]) assume every key already has one.
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

    #[error("cursor token is not valid base64")]
    TokenInvalidBase64,

    #[error("truncated cursor token")]
    TokenTruncated,

    #[error("cursor token has trailing bytes after its last key")]
    TokenTrailingBytes,

    #[error("unsupported cursor token version {0}")]
    TokenUnsupportedVersion(u8),

    #[error("cursor token checksum mismatch — the token was corrupted or tampered with")]
    TokenChecksumMismatch,

    #[error("cursor token contains invalid UTF-8")]
    TokenInvalidUtf8,

    #[error("unknown cursor value tag 0x{0:02x}")]
    TokenUnknownValueTag(u8),

    #[error("unknown cursor sort direction byte 0x{0:02x}")]
    TokenUnknownDirection(u8),

    #[error("cursor token contains an out-of-range timestamp")]
    TokenInvalidTimestamp,
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
    /// type's public API, since only [`after`](Self::after) and
    /// [`parse`](Self::parse) set values, and both always set all of them
    /// together.
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

    /// Encodes this cursor as an opaque `page_token` string, safe to hand
    /// back to the client as-is. See the module docs for the wire format
    /// and what the checksum does and doesn't guard against.
    ///
    /// Panics if any key's value is unset — unreachable through this
    /// type's public API, same as [`to_where_clause`](Self::to_where_clause).
    pub fn encode(&self) -> String {
        let body = encode_body(&self.keys);

        let mut for_checksum = Vec::with_capacity(1 + body.len());
        for_checksum.push(CURSOR_TOKEN_VERSION);
        for_checksum.extend_from_slice(&body);
        let checksum = crc32(&for_checksum);

        let mut wire = Vec::with_capacity(5 + body.len());
        wire.push(CURSOR_TOKEN_VERSION);
        wire.extend_from_slice(&checksum.to_be_bytes());
        wire.extend_from_slice(&body);

        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(wire)
    }

    /// Decodes a `page_token` string produced by [`encode`](Self::encode)
    /// back into a `Cursor` carrying both its `order_by` and its boundary
    /// values — no separate [`new`](Self::new)/[`after`](Self::after) call
    /// needed, unlike building one from scratch.
    pub fn parse(token: &str) -> Result<Self, CursorError> {
        let wire = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(token)
            .map_err(|_| CursorError::TokenInvalidBase64)?;

        let mut reader = Reader { buf: &wire };
        let version = reader.read_u8()?;
        if version != CURSOR_TOKEN_VERSION {
            return Err(CursorError::TokenUnsupportedVersion(version));
        }
        let stored_checksum = reader.read_u32()?;
        let body = reader.buf;

        let mut for_checksum = Vec::with_capacity(1 + body.len());
        for_checksum.push(version);
        for_checksum.extend_from_slice(body);
        if crc32(&for_checksum) != stored_checksum {
            return Err(CursorError::TokenChecksumMismatch);
        }

        let keys = decode_body(Reader { buf: body })?;
        Ok(Cursor { keys })
    }
}

/// `key`'s 1-based position in `keys` — its placeholder number, since
/// each key's value is bound once and referenced by number wherever it
/// recurs across the OR branches.
fn placeholder_index(keys: &[CursorKey], key: &CursorKey) -> usize {
    keys.iter().position(|k| std::ptr::eq(k, key)).unwrap() + 1
}

/// The leading byte of every encoded cursor token. Bump this whenever the
/// encoding changes in a way that would make an already-issued token
/// decode to something different — [`Cursor::parse`] rejects any other
/// version outright rather than misreading it.
const CURSOR_TOKEN_VERSION: u8 = 1;

// `Value` variant tags. Wire format — never renumber these; append new
// ones instead.
const TAG_NULL: u8 = 0;
const TAG_BOOL: u8 = 1;
const TAG_INT: u8 = 2;
const TAG_FLOAT: u8 = 3;
const TAG_STRING: u8 = 4;
const TAG_TIMESTAMP: u8 = 5;

fn direction_byte(direction: OrderDirection) -> u8 {
    match direction {
        OrderDirection::Asc => 0,
        OrderDirection::Desc => 1,
    }
}

fn direction_from_byte(byte: u8) -> Result<OrderDirection, CursorError> {
    match byte {
        0 => Ok(OrderDirection::Asc),
        1 => Ok(OrderDirection::Desc),
        other => Err(CursorError::TokenUnknownDirection(other)),
    }
}

fn encode_body(keys: &[CursorKey]) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&(keys.len() as u32).to_be_bytes());
    for key in keys {
        let column = key.key.column().as_bytes();
        body.extend_from_slice(&(column.len() as u32).to_be_bytes());
        body.extend_from_slice(column);
        body.push(direction_byte(key.key.direction()));
        encode_value(
            &mut body,
            key.value
                .as_ref()
                .expect("Cursor::encode called before after() set all values"),
        );
    }
    body
}

fn encode_value(buf: &mut Vec<u8>, value: &Value) {
    match value {
        Value::Null => buf.push(TAG_NULL),
        Value::Bool(v) => {
            buf.push(TAG_BOOL);
            buf.push(*v as u8);
        }
        Value::Int(v) => {
            buf.push(TAG_INT);
            buf.extend_from_slice(&v.to_be_bytes());
        }
        Value::Float(v) => {
            buf.push(TAG_FLOAT);
            buf.extend_from_slice(&v.to_bits().to_be_bytes());
        }
        Value::String(v) => {
            buf.push(TAG_STRING);
            buf.extend_from_slice(&(v.len() as u32).to_be_bytes());
            buf.extend_from_slice(v.as_bytes());
        }
        Value::Timestamp(v) => {
            buf.push(TAG_TIMESTAMP);
            buf.extend_from_slice(&v.timestamp().to_be_bytes());
            buf.extend_from_slice(&v.timestamp_subsec_nanos().to_be_bytes());
        }
    }
}

fn decode_body(mut reader: Reader<'_>) -> Result<Vec<CursorKey>, CursorError> {
    let count = reader.read_u32()? as usize;
    let mut keys = Vec::with_capacity(count);
    for _ in 0..count {
        let len = reader.read_u32()? as usize;
        let column = reader.read_string(len)?;
        let direction = direction_from_byte(reader.read_u8()?)?;
        let value = decode_value(&mut reader)?;
        keys.push(CursorKey {
            key: OrderKey::new(column, direction),
            value: Some(value),
        });
    }
    if !reader.buf.is_empty() {
        return Err(CursorError::TokenTrailingBytes);
    }
    Ok(keys)
}

fn decode_value(reader: &mut Reader<'_>) -> Result<Value, CursorError> {
    match reader.read_u8()? {
        TAG_NULL => Ok(Value::Null),
        TAG_BOOL => Ok(Value::Bool(reader.read_u8()? != 0)),
        TAG_INT => Ok(Value::Int(reader.read_i64()?)),
        TAG_FLOAT => Ok(Value::Float(f64::from_bits(reader.read_u64()?))),
        TAG_STRING => {
            let len = reader.read_u32()? as usize;
            Ok(Value::String(reader.read_string(len)?))
        }
        TAG_TIMESTAMP => {
            let secs = reader.read_i64()?;
            let nanos = reader.read_u32()?;
            let timestamp: DateTime<Utc> =
                DateTime::from_timestamp(secs, nanos).ok_or(CursorError::TokenInvalidTimestamp)?;
            Ok(Value::Timestamp(timestamp))
        }
        other => Err(CursorError::TokenUnknownValueTag(other)),
    }
}

/// A cursor over an in-memory byte slice, with bounds-checked reads —
/// every [`Cursor::parse`] failure mode below `TokenInvalidBase64` and
/// `TokenUnsupportedVersion` funnels through this instead of each call
/// site checking `buf.len()` by hand.
struct Reader<'a> {
    buf: &'a [u8],
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], CursorError> {
        if self.buf.len() < n {
            return Err(CursorError::TokenTruncated);
        }
        let (head, rest) = self.buf.split_at(n);
        self.buf = rest;
        Ok(head)
    }

    fn read_u8(&mut self) -> Result<u8, CursorError> {
        Ok(self.take(1)?[0])
    }

    fn read_u32(&mut self) -> Result<u32, CursorError> {
        Ok(u32::from_be_bytes(self.take(4)?.try_into().unwrap()))
    }

    fn read_u64(&mut self) -> Result<u64, CursorError> {
        Ok(u64::from_be_bytes(self.take(8)?.try_into().unwrap()))
    }

    fn read_i64(&mut self) -> Result<i64, CursorError> {
        Ok(i64::from_be_bytes(self.take(8)?.try_into().unwrap()))
    }

    fn read_string(&mut self, len: usize) -> Result<String, CursorError> {
        String::from_utf8(self.take(len)?.to_vec()).map_err(|_| CursorError::TokenInvalidUtf8)
    }
}

/// IEEE CRC32 (the same variant Go's `hash/crc32.ChecksumIEEE` computes),
/// hand-rolled bitwise rather than table-driven — this only ever runs once
/// per `encode`/`parse` call over a few dozen bytes, so the table's
/// speedup isn't worth the extra static data. Verified against the
/// standard `"123456789"` -> `0xCBF43926` test vector below.
fn crc32(bytes: &[u8]) -> u32 {
    const POLY: u32 = 0xEDB88320;
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
        let timestamp = DateTime::from_timestamp(1_700_000_000, 123_000_000).unwrap();
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
    fn parse_rejects_truncated_token() {
        let token = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode([1u8, 2, 3]);
        let err = Cursor::parse(&token).unwrap_err();
        assert_eq!(err, CursorError::TokenTruncated);
    }

    #[test]
    fn parse_rejects_unsupported_version() {
        let mut wire = vec![99u8];
        let checksum = crc32(&wire);
        wire.extend_from_slice(&checksum.to_be_bytes());
        let token = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&wire);

        let err = Cursor::parse(&token).unwrap_err();
        assert_eq!(err, CursorError::TokenUnsupportedVersion(99));
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
        assert_eq!(err, CursorError::TokenChecksumMismatch);
    }

    #[test]
    fn parse_rejects_trailing_bytes() {
        let order_by = OrderByClause::parse("rank desc").unwrap();
        let cursor = Cursor::new(order_by).after(vec![Value::Int(42)]).unwrap();
        let token = cursor.encode();

        let mut wire = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(&token)
            .unwrap();
        wire.push(0);
        let version = wire[0];
        let body = wire[5..].to_vec();
        let mut for_checksum = vec![version];
        for_checksum.extend_from_slice(&body);
        let checksum = crc32(&for_checksum);
        wire[1..5].copy_from_slice(&checksum.to_be_bytes());
        let padded = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&wire);

        let err = Cursor::parse(&padded).unwrap_err();
        assert_eq!(err, CursorError::TokenTrailingBytes);
    }
}
