//! Page tokens: where the last page stopped, and under what ordering.

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as BASE64;
use chrono::{TimeZone as _, Utc};

use crate::dialect::{Dialect, reference};
use crate::error::Error;
use crate::fragment::QueryFragment;
use crate::mapping::{Column, Mapping};
use crate::sort::{Direction, Sort, SortKey};
use crate::value::Value;
use sqlx::Row;

/// The format this build writes.
///
/// Clients persist tokens and hand them back days later, across deploys, so a
/// format change has to be detectable. Without this byte an old token decodes
/// into plausible nonsense instead of being refused, which is the one mistake
/// here that cannot be corrected afterwards.
const VERSION: u8 = 1;

const TAG_BOOL: u8 = 0;
const TAG_INT: u8 = 1;
const TAG_FLOAT: u8 = 2;
const TAG_TEXT: u8 = 3;
const TAG_BYTES: u8 = 4;
const TAG_TIMESTAMP: u8 = 5;

const DIR_ASC: u8 = 0;
const DIR_DESC: u8 = 1;

/// One component of a position: a sort key and the value the last row had for
/// it.
///
/// Pairing them is what makes a key without a value unrepresentable. Two
/// parallel lists would let them drift, and drift here means seeking to a
/// position that does not exist.
#[derive(Debug, Clone, PartialEq)]
pub struct CursorKey {
    /// The field and direction this component orders by.
    pub key: SortKey,
    /// What the last row of the previous page held there.
    pub value: Value,
}

/// Where a page stopped.
///
/// # What a token carries
///
/// The key values, and the ordering they were taken under. The ordering is
/// recorded because a client that changes `order_by` between pages and reuses
/// the token would otherwise get a page that looks fine and is wrong -- rows
/// already seen, rows never seen. The builder compares the recorded ordering
/// against the one it is given and refuses a mismatch.
///
/// # What it does not carry
///
/// Any claim to integrity. A token is base64, not a signature: anyone can
/// decode one, change both the values and the recorded ordering, and encode it
/// again. The recorded ordering is a check against a confused client, not a
/// hostile one.
///
/// That is usually enough, because a forged cursor only seeks within a result
/// set the caller could already page through. It stops being enough if seeking
/// is itself a way to learn something -- then the token wants signing, and the
/// version byte is where that would go.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Cursor {
    /// The ordering this cursor pages by.
    sort: Sort,
    /// Where in that ordering, or empty for the start of the listing.
    ///
    /// When non-empty there is exactly one per key of `sort`, and they carry
    /// the same keys: [`after`](Self::after) builds them by zipping against
    /// `sort`, and [`parse`](Self::parse) derives `sort` from them, so neither
    /// constructor can set the two independently.
    keys: Vec<CursorKey>,
    /// Kept alongside so [`as_str`](Self::as_str) can borrow rather than
    /// re-encode on every page.
    token: String,
}

impl Cursor {
    /// A cursor that pages by `sort`, at the start of the listing.
    ///
    /// The field names and directions come from `sort` rather than from the
    /// caller, so a token cannot record an ordering the query did not run
    /// under.
    #[must_use]
    pub fn new(sort: &Sort) -> Self {
        Self {
            sort: sort.clone(),
            keys: Vec::new(),
            token: String::new(),
        }
    }

    /// A cursor positioned after a row whose key values are already in hand.
    ///
    /// Not public: the values are positional, so nothing here can check that
    /// they are in the ordering's key order -- only that there are the right
    /// number of them. Transposing two of the same type would mint a token
    /// that seeks somewhere the caller never was. [`after`](Self::after) reads
    /// them by name instead, which cannot go wrong that way.
    pub(crate) fn after_values(&self, key_values: &[Value]) -> Result<Self, Error> {
        if self.sort.keys().len() != key_values.len() {
            return Err(Error::Cursor(format!(
                "the ordering has {} keys but {} values were given",
                self.sort.keys().len(),
                key_values.len()
            )));
        }

        let keys: Vec<CursorKey> = self
            .sort
            .keys()
            .iter()
            .zip(key_values)
            .map(|(key, value)| CursorKey {
                key: key.clone(),
                value: value.clone(),
            })
            .collect();

        let token = BASE64.encode(encode(&keys));

        Ok(Self {
            sort: self.sort.clone(),
            keys,
            token,
        })
    }

    /// A cursor positioned after `row`.
    ///
    /// Named for the boundary: the next page holds rows strictly *after* this
    /// one, which is excluded. Pass the last row of the page just sent and the
    /// result is that page's token.
    ///
    /// Each key's value is read from the column `mapping` maps it to, so there
    /// is no second field-to-column mapping for a caller to keep in step, and
    /// the ordering may be one the client chose at runtime.
    ///
    /// # Errors
    ///
    /// [`Error::UnknownColumn`] for a key the mapping does not expose, and
    /// [`Error::Column`] if a key's column is not in the row -- usually because
    /// it was left out of the `SELECT` list.
    pub fn after<R>(&self, row: &R) -> Result<Self, Error>
    where
        R: Row,
        R::Database: Dialect,
    {
        let values = self
            .sort
            .columns()?
            .iter()
            .map(|column| <R::Database as Dialect>::value(row, column.result_name(), column.ty))
            .collect::<Result<Vec<_>, _>>()?;

        self.after_values(&values)
    }

    /// Resolve the ordering this cursor pages by, through `mapping`.
    ///
    /// The same boundary [`Sort::resolve`] is: afterwards the seek condition
    /// renders, and rows are read, without the mapping again.
    ///
    /// # Errors
    ///
    /// [`Error::UnknownColumn`] for a key the mapping does not expose.
    pub fn resolve(mut self, mapping: &dyn Mapping) -> Result<Self, Error> {
        self.sort = self.sort.resolve(mapping)?;
        Ok(self)
    }

    /// Read a page token.
    ///
    /// An empty token is the first page, not an error -- that is what a client
    /// starting a list sends.
    ///
    /// # Errors
    ///
    /// [`Error::Cursor`] if the token is not base64, was written by a different
    /// version of this format, or is truncated or otherwise malformed.
    pub fn parse(token: &str) -> Result<Self, Error> {
        if token.trim().is_empty() {
            return Ok(Self::default());
        }

        let bytes = BASE64
            .decode(token)
            .map_err(|error| Error::Cursor(format!("not base64url: {error}")))?;

        let keys = decode(&bytes)?;

        Ok(Self {
            sort: keys.iter().map(|key| key.key.clone()).collect(),
            keys,
            token: token.to_owned(),
        })
    }

    /// The encoded token, to hand back to the client.
    ///
    /// Empty for the first page, which is exactly what a client should send to
    /// start over.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.token
    }

    /// The position, key by key.
    #[must_use]
    pub fn keys(&self) -> &[CursorKey] {
        &self.keys
    }

    /// Whether this is the first page.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    /// The condition selecting rows after this position.
    ///
    /// Empty on the first page, so the slot it fills -- and that slot's joiner
    /// -- disappear from the query entirely.
    ///
    /// # Errors
    ///
    /// [`Error::UnknownColumn`] for a key the mapping does not expose,
    /// [`Error::NotUnique`] if no key is a unique column, and [`Error::Cursor`]
    /// if a value's type does not match its column's.
    pub(crate) fn to_fragment<DB: Dialect>(&self) -> Result<QueryFragment<DB, Value>, Error> {
        seek(&self.keys, self.sort.columns()?)
    }

    /// The ordering this cursor pages by.
    ///
    /// For a parsed token, the ordering it was issued under.
    #[must_use]
    pub fn sort(&self) -> &Sort {
        &self.sort
    }
}

/// Render the rows-after-a-position condition.
///
/// Always an OR-chain, never a row-value comparison like `(a, b) > (x, y)`. A
/// row value only works when every key runs the same way, so `title asc, id
/// desc` has to be written out:
///
/// ```text
/// (("title" > $1) OR ("title" = $2 AND "id" < $3))
/// ```
///
/// Writing it out always, rather than only when directions are mixed, costs a
/// few repeated binds and buys one shape to test and no driver-specific branch.
/// A positional `?` cannot point back at an earlier bind, so the repetition is
/// unavoidable on those drivers regardless.
fn seek<DB: Dialect>(
    keys: &[CursorKey],
    columns: &[Column],
) -> Result<QueryFragment<DB, Value>, Error> {
    let mut fragment = QueryFragment::new();
    if keys.is_empty() {
        return Ok(fragment);
    }

    let mut total = false;

    for (key, column) in keys.iter().zip(columns) {
        // A token is client input. One carrying text for an integer column must
        // not reach the database as a comparison between the two.
        if column.ty != key.value.ty() {
            return Err(Error::Cursor(format!(
                "holds {} for `{}`, which is {}",
                key.value.ty(),
                key.key.field,
                column.ty
            )));
        }

        total |= column.unique;
    }

    if !total {
        let sort: Sort = keys.iter().map(|key| key.key.clone()).collect();

        return Err(Error::NotUnique(format!(
            "`{sort}` names no unique column, so a page token cannot identify a \
             row: append one with `Sort::asc`, and declare it with `QueryMapping::key`"
        )));
    }

    fragment.push("(");

    for (at, key) in keys.iter().enumerate() {
        if at > 0 {
            fragment.push(" OR ");
        }
        fragment.push("(");

        for (before, earlier) in keys.iter().enumerate().take(at) {
            fragment.push(&reference::<DB>(&columns[before]));
            fragment.push(" = ");
            fragment.push_bind(earlier.value.clone());
            fragment.push(" AND ");
        }

        fragment.push(&reference::<DB>(&columns[at]));
        fragment.push(match key.key.direction {
            Direction::Asc => " > ",
            Direction::Desc => " < ",
        });
        fragment.push_bind(key.value.clone());

        fragment.push(")");
    }

    fragment.push(")");

    Ok(fragment)
}

fn encode(keys: &[CursorKey]) -> Vec<u8> {
    let mut out = vec![VERSION];

    for CursorKey { key, value } in keys {
        out.push(match key.direction {
            Direction::Asc => DIR_ASC,
            Direction::Desc => DIR_DESC,
        });

        write_bytes(&mut out, key.field.as_bytes());

        match value {
            Value::Bool(value) => {
                out.push(TAG_BOOL);
                out.push(u8::from(*value));
            }
            Value::Int(value) => {
                out.push(TAG_INT);
                out.extend_from_slice(&value.to_be_bytes());
            }
            Value::Float(value) => {
                out.push(TAG_FLOAT);
                out.extend_from_slice(&value.to_bits().to_be_bytes());
            }
            Value::Text(value) => {
                out.push(TAG_TEXT);
                write_bytes(&mut out, value.as_bytes());
            }
            Value::Bytes(value) => {
                out.push(TAG_BYTES);
                write_bytes(&mut out, value);
            }
            Value::Timestamp(value) => {
                // Seconds and nanos rather than a single count of nanoseconds,
                // which would silently clamp the range to 1677..2262.
                out.push(TAG_TIMESTAMP);
                out.extend_from_slice(&value.timestamp().to_be_bytes());
                out.extend_from_slice(&value.timestamp_subsec_nanos().to_be_bytes());
            }
        }
    }

    out
}

fn write_bytes(out: &mut Vec<u8>, bytes: &[u8]) {
    let len = u32::try_from(bytes.len()).unwrap_or(u32::MAX);
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(&bytes[..len as usize]);
}

fn decode(bytes: &[u8]) -> Result<Vec<CursorKey>, Error> {
    let mut reader = Reader { bytes, at: 0 };

    match reader.byte()? {
        VERSION => {}
        other => {
            return Err(Error::Cursor(format!(
                "written by version {other} of this format, which this build \
                 does not read"
            )));
        }
    }

    let mut keys = Vec::new();
    while !reader.done() {
        let direction = match reader.byte()? {
            DIR_ASC => Direction::Asc,
            DIR_DESC => Direction::Desc,
            other => return Err(Error::Cursor(format!("unknown direction {other}"))),
        };

        let field = reader.string()?;

        let value = match reader.byte()? {
            TAG_BOOL => Value::Bool(reader.byte()? != 0),
            TAG_INT => Value::Int(i64::from_be_bytes(reader.array()?)),
            TAG_FLOAT => Value::Float(f64::from_bits(u64::from_be_bytes(reader.array()?))),
            TAG_TEXT => Value::Text(reader.string()?),
            TAG_BYTES => Value::Bytes(reader.bytes()?.to_vec()),
            TAG_TIMESTAMP => {
                let seconds = i64::from_be_bytes(reader.array()?);
                let nanos = u32::from_be_bytes(reader.array()?);

                Value::Timestamp(
                    Utc.timestamp_opt(seconds, nanos)
                        .single()
                        .ok_or_else(|| Error::Cursor("timestamp out of range".to_owned()))?,
                )
            }
            other => return Err(Error::Cursor(format!("unknown value type {other}"))),
        };

        keys.push(CursorKey {
            key: SortKey { field, direction },
            value,
        });
    }

    Ok(keys)
}

/// A bounds-checked walk over a token.
///
/// Tokens come from clients, so every read has to assume the bytes are wrong:
/// truncated, reordered, or made up entirely.
struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    fn done(&self) -> bool {
        self.at >= self.bytes.len()
    }

    fn byte(&mut self) -> Result<u8, Error> {
        let byte = *self.bytes.get(self.at).ok_or_else(truncated)?;
        self.at += 1;
        Ok(byte)
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N], Error> {
        let slice = self.take(N)?;
        slice.try_into().map_err(|_| truncated())
    }

    fn bytes(&mut self) -> Result<&'a [u8], Error> {
        let len = u32::from_be_bytes(self.array()?) as usize;
        self.take(len)
    }

    fn string(&mut self) -> Result<String, Error> {
        let bytes = self.bytes()?;

        String::from_utf8(bytes.to_vec())
            .map_err(|_| Error::Cursor("contains text that is not UTF-8".to_owned()))
    }

    fn take(&mut self, len: usize) -> Result<&'a [u8], Error> {
        let end = self.at.checked_add(len).ok_or_else(truncated)?;
        let slice = self.bytes.get(self.at..end).ok_or_else(truncated)?;
        self.at = end;
        Ok(slice)
    }
}

fn truncated() -> Error {
    Error::Cursor("ends in the middle of a value".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sort() -> Sort {
        Sort::parse("title desc, id").unwrap()
    }

    #[test]
    fn the_first_page_has_an_empty_token() {
        let cursor = Cursor::parse("").unwrap();

        assert!(cursor.is_empty());
        assert_eq!(cursor.as_str(), "");
    }

    /// An empty token round-trips to an empty cursor, so a client starting a
    /// list and a client that has finished one look the same.
    #[test]
    fn an_empty_token_parses_to_the_first_page() {
        assert!(Cursor::parse("").unwrap().is_empty());
        assert!(Cursor::parse("   ").unwrap().is_empty());
    }

    #[test]
    fn a_position_round_trips_through_a_token() {
        let values = [Value::Text("Dune".into()), Value::Int(4711)];
        let cursor = Cursor::new(&sort()).after_values(&values).unwrap();

        let parsed = Cursor::parse(cursor.as_str()).unwrap();

        assert_eq!(parsed.keys(), cursor.keys());
        assert_eq!(parsed.sort(), &sort());
    }

    #[test]
    fn every_value_type_round_trips() {
        let sort = Sort::parse("a, b, c, d, e, f").unwrap();
        let values = [
            Value::Bool(true),
            Value::Int(-9_000_000_000),
            Value::Float(1.5),
            Value::Text("caf\u{e9} \u{1f600}".into()),
            Value::Bytes(vec![0, 255, 7]),
            Value::Timestamp(Utc.timestamp_opt(1_700_000_000, 123_456_789).unwrap()),
        ];

        let cursor = Cursor::new(&sort).after_values(&values).unwrap();
        let parsed = Cursor::parse(cursor.as_str()).unwrap();

        let round_tripped: Vec<Value> = parsed.keys().iter().map(|k| k.value.clone()).collect();
        assert_eq!(round_tripped, values);
    }

    /// Dates outside 1677..2262 are why the timestamp is seconds plus nanos
    /// rather than one count of nanoseconds.
    #[test]
    fn timestamps_outside_the_nanosecond_range_survive() {
        let sort = Sort::parse("at").unwrap();
        let far = Utc.timestamp_opt(99_999_999_999, 0).unwrap();

        let cursor = Cursor::new(&sort)
            .after_values(&[Value::Timestamp(far)])
            .unwrap();
        let parsed = Cursor::parse(cursor.as_str()).unwrap();

        assert_eq!(parsed.keys()[0].value, Value::Timestamp(far));
    }

    #[test]
    fn the_recorded_direction_survives() {
        let cursor = Cursor::new(&sort())
            .after_values(&[Value::Text("x".into()), Value::Int(1)])
            .unwrap();
        let parsed = Cursor::parse(cursor.as_str()).unwrap();

        assert_eq!(parsed.keys()[0].key.direction, Direction::Desc);
        assert_eq!(parsed.keys()[1].key.direction, Direction::Asc);
    }

    #[test]
    fn a_value_count_that_does_not_match_the_sort_is_rejected() {
        let error = Cursor::new(&sort())
            .after_values(&[Value::Int(1)])
            .unwrap_err();

        assert!(
            format!("{error}").contains("2 keys but 1 values"),
            "{error}"
        );
    }

    /// Tokens are client input, so every malformed shape has to be refused
    /// rather than read past the end of the buffer.
    #[test]
    fn malformed_tokens_are_rejected() {
        let good = Cursor::new(&sort())
            .after_values(&[Value::Text("x".into()), Value::Int(1)])
            .unwrap()
            .as_str()
            .to_owned();

        let truncated = &good[..good.len() - 4];
        assert!(Cursor::parse(truncated).is_err(), "accepted a truncation");

        assert!(
            Cursor::parse("not base64!!").is_err(),
            "accepted non-base64"
        );

        let wrong_version = BASE64.encode([99_u8, 0, 0, 0, 0, 1, b'a']);
        let error = Cursor::parse(&wrong_version).unwrap_err();
        assert!(format!("{error}").contains("version 99"), "{error}");
    }

    #[cfg(feature = "postgres")]
    #[test]
    fn the_first_page_has_no_condition() {
        let fragment = Cursor::parse("")
            .unwrap()
            .to_fragment::<sqlx::Postgres>()
            .unwrap();

        assert!(fragment.is_empty());
    }

    /// A client that pages under one order and then asks for another gets an
    /// An omitted `order_by` is a changed parameter like any other: the page
    /// The ordering that renders the ORDER BY and the one the condition is
    /// A URL is where these end up, so the alphabet matters.
    #[test]
    fn tokens_are_url_safe() {
        let sort = Sort::parse("blob").unwrap();
        let value = Value::Bytes((0..=255).collect());

        let token = Cursor::new(&sort)
            .after_values(&[value])
            .unwrap()
            .as_str()
            .to_owned();

        assert!(
            token
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
            "{token}"
        );
    }
}
