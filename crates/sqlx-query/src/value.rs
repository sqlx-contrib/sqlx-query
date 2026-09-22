use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::{Column, ColumnIndex, Decode, Encode, Row, Type, ValueRef};

use crate::CursorError;

/// A dialect-agnostic bind value. [`WhereClause`](crate::WhereClause) and
/// [`OrderByClause`](crate::OrderByClause) carry these instead of binding
/// directly against a concrete `sqlx::Database`, so a fragment built
/// elsewhere (e.g. `sqlx-query-cel`'s `FilterClause`, which converts
/// into a `WhereClause`) can be spliced into a query for any dialect
/// [`QueryComposer`](crate::QueryComposer) supports.
///
/// `Serialize`/`Deserialize` (via chrono's `serde` feature for
/// `Timestamp`) are what let [`Cursor`](crate::Cursor) derive them too,
/// for [`Cursor::encode`](crate::Cursor::encode)/[`Cursor::parse`](crate::Cursor::parse)'s
/// token format — see that method's docs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Value {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    String(String),
    Timestamp(DateTime<Utc>),
    /// A `BLOB`/`bytea`, which completes SQLite's storage classes:
    /// null, integer, real, text, blob.
    ///
    /// On the other dialects it's one binary type among many. `Value` is
    /// closed and can't be extended from outside this crate, so a
    /// parameter it can't hold — a `Uuid`, a `serde_json::Value`, a
    /// decimal — has to be converted by the caller. On PostgreSQL that
    /// only works where the server accepts the converted form: bytes
    /// bound to a `uuid` column are rejected, bytes bound to a `bytea`
    /// are not.
    Bytes(Vec<u8>),
}

impl From<bool> for Value {
    fn from(value: bool) -> Self {
        Value::Bool(value)
    }
}

macro_rules! impl_from_int {
    ($($ty:ty),* $(,)?) => {
        $(
            impl From<$ty> for Value {
                fn from(value: $ty) -> Self {
                    Value::Int(i64::from(value))
                }
            }
        )*
    };
}

impl_from_int!(i8, i16, i32, i64, u8, u16, u32);

macro_rules! impl_from_float {
    ($($ty:ty),* $(,)?) => {
        $(
            impl From<$ty> for Value {
                fn from(value: $ty) -> Self {
                    Value::Float(f64::from(value))
                }
            }
        )*
    };
}

impl_from_float!(f32, f64);

impl From<String> for Value {
    fn from(value: String) -> Self {
        Value::String(value)
    }
}

impl From<&str> for Value {
    fn from(value: &str) -> Self {
        Value::String(value.to_owned())
    }
}

impl From<Vec<u8>> for Value {
    fn from(value: Vec<u8>) -> Self {
        Value::Bytes(value)
    }
}

impl From<&[u8]> for Value {
    fn from(value: &[u8]) -> Self {
        Value::Bytes(value.to_vec())
    }
}

impl From<DateTime<Utc>> for Value {
    fn from(value: DateTime<Utc>) -> Self {
        Value::Timestamp(value)
    }
}

impl<T> From<Option<T>> for Value
where
    T: Into<Value>,
{
    fn from(value: Option<T>) -> Self {
        match value {
            Some(value) => value.into(),
            None => Value::Null,
        }
    }
}

/// `Value` is a union of types sqlx already knows how to encode, so
/// [`QueryComposer::build`](crate::QueryComposer::build) binds a `Value`
/// directly rather than matching the variant and binding each separately.
///
/// [`type_info`](Type::type_info) has no value to look at, so it can't
/// answer honestly — it names one variant's type arbitrarily and
/// [`compatible`](Type::compatible) accepts everything. The honest answer
/// comes from [`Encode::produces`], which *does* see the value, and which
/// is what a driver consults when it needs a parameter's type (PostgreSQL
/// sends one per bind). That split is sqlx's own escape hatch for a type
/// whose SQL type depends on the value.
impl<DB> Type<DB> for Value
where
    DB: sqlx::Database,
    bool: Type<DB>,
    i64: Type<DB>,
    f64: Type<DB>,
    String: Type<DB>,
    DateTime<Utc>: Type<DB>,
    Vec<u8>: Type<DB>,
{
    fn type_info() -> DB::TypeInfo {
        <String as Type<DB>>::type_info()
    }

    fn compatible(_: &DB::TypeInfo) -> bool {
        true
    }
}

impl<'q, DB> Encode<'q, DB> for Value
where
    DB: sqlx::Database,
    bool: Encode<'q, DB> + Type<DB>,
    i64: Encode<'q, DB> + Type<DB>,
    f64: Encode<'q, DB> + Type<DB>,
    String: Encode<'q, DB> + Type<DB>,
    DateTime<Utc>: Encode<'q, DB> + Type<DB>,
    Vec<u8>: Encode<'q, DB> + Type<DB>,
    Option<String>: Encode<'q, DB> + Type<DB>,
{
    fn encode_by_ref(
        &self,
        buf: &mut <DB as sqlx::Database>::ArgumentBuffer,
    ) -> Result<sqlx::encode::IsNull, sqlx::error::BoxDynError> {
        match self {
            Value::Null => <Option<String> as Encode<'q, DB>>::encode_by_ref(&None, buf),
            Value::Bool(v) => <bool as Encode<'q, DB>>::encode_by_ref(v, buf),
            Value::Int(v) => <i64 as Encode<'q, DB>>::encode_by_ref(v, buf),
            Value::Float(v) => <f64 as Encode<'q, DB>>::encode_by_ref(v, buf),
            Value::String(v) => <String as Encode<'q, DB>>::encode_by_ref(v, buf),
            Value::Timestamp(v) => <DateTime<Utc> as Encode<'q, DB>>::encode_by_ref(v, buf),
            Value::Bytes(v) => <Vec<u8> as Encode<'q, DB>>::encode_by_ref(v, buf),
        }
    }

    fn produces(&self) -> Option<DB::TypeInfo> {
        Some(match self {
            Value::Null => <Option<String> as Type<DB>>::type_info(),
            Value::Bool(_) => <bool as Type<DB>>::type_info(),
            Value::Int(_) => <i64 as Type<DB>>::type_info(),
            Value::Float(_) => <f64 as Type<DB>>::type_info(),
            Value::String(_) => <String as Type<DB>>::type_info(),
            Value::Timestamp(_) => <DateTime<Utc> as Type<DB>>::type_info(),
            Value::Bytes(_) => <Vec<u8> as Type<DB>>::type_info(),
        })
    }
}

/// Extends any `sqlx::Row` with the ability to read one column as a
/// dialect-agnostic [`Value`], for
/// [`Cursor::after_row`](crate::Cursor::after_row) — `row.get_value(col)`
/// rather than `Value::from_row(row, col)`, matching `sqlx`'s own
/// `row.try_get`/`row.get` naming instead of reading backwards as "ask
/// `Value` to reach into a row."
pub(crate) trait RowExtension: Row {
    /// `column` may be table-qualified (`"v.created_at"`, from a resolved
    /// `order_by`), but the row's own column only ever carries the
    /// unqualified label (`"created_at"`) — so this matches on the segment
    /// after the last `.`.
    ///
    /// The column's SQL type isn't known to this crate ahead of time, so
    /// rather than re-deriving a dialect-specific OID/type-code table, this
    /// just tries each candidate Rust type in turn and keeps whichever one
    /// `sqlx`'s own `Type::compatible` check accepts — `sqlx` already
    /// carries that per-driver knowledge, so there's no reason to
    /// duplicate it here.
    fn get_value<'r>(&'r self, column: &str) -> Result<Value, CursorError>
    where
        usize: ColumnIndex<Self>,
        bool: Decode<'r, Self::Database> + Type<Self::Database>,
        i16: Decode<'r, Self::Database> + Type<Self::Database>,
        i32: Decode<'r, Self::Database> + Type<Self::Database>,
        i64: Decode<'r, Self::Database> + Type<Self::Database>,
        f32: Decode<'r, Self::Database> + Type<Self::Database>,
        f64: Decode<'r, Self::Database> + Type<Self::Database>,
        String: Decode<'r, Self::Database> + Type<Self::Database>,
        DateTime<Utc>: Decode<'r, Self::Database> + Type<Self::Database>,
        Vec<u8>: Decode<'r, Self::Database> + Type<Self::Database>;
}

impl<R: Row> RowExtension for R {
    fn get_value<'r>(&'r self, column: &str) -> Result<Value, CursorError>
    where
        usize: ColumnIndex<Self>,
        bool: Decode<'r, Self::Database> + Type<Self::Database>,
        i16: Decode<'r, Self::Database> + Type<Self::Database>,
        i32: Decode<'r, Self::Database> + Type<Self::Database>,
        i64: Decode<'r, Self::Database> + Type<Self::Database>,
        f32: Decode<'r, Self::Database> + Type<Self::Database>,
        f64: Decode<'r, Self::Database> + Type<Self::Database>,
        String: Decode<'r, Self::Database> + Type<Self::Database>,
        DateTime<Utc>: Decode<'r, Self::Database> + Type<Self::Database>,
        Vec<u8>: Decode<'r, Self::Database> + Type<Self::Database>,
    {
        let label = column.rsplit('.').next().unwrap_or(column);
        let ordinal = self
            .columns()
            .iter()
            .find(|c| c.name() == label)
            .map(Column::ordinal)
            .ok_or_else(|| CursorError::RowColumnMissing(label.to_owned()))?;

        let is_null = self
            .try_get_raw(ordinal)
            .map_err(|_| CursorError::RowColumnMissing(label.to_owned()))?
            .is_null();
        if is_null {
            return Ok(Value::Null);
        }

        if let Ok(v) = self.try_get::<i64, _>(ordinal) {
            return Ok(Value::Int(v));
        }
        if let Ok(v) = self.try_get::<i32, _>(ordinal) {
            return Ok(Value::Int(v.into()));
        }
        if let Ok(v) = self.try_get::<i16, _>(ordinal) {
            return Ok(Value::Int(v.into()));
        }
        if let Ok(v) = self.try_get::<f64, _>(ordinal) {
            return Ok(Value::Float(v));
        }
        if let Ok(v) = self.try_get::<f32, _>(ordinal) {
            return Ok(Value::Float(v.into()));
        }
        if let Ok(v) = self.try_get::<bool, _>(ordinal) {
            return Ok(Value::Bool(v));
        }
        if let Ok(v) = self.try_get::<DateTime<Utc>, _>(ordinal) {
            return Ok(Value::Timestamp(v));
        }
        if let Ok(v) = self.try_get::<String, _>(ordinal) {
            return Ok(Value::String(v));
        }
        // Last: on some drivers a text column will also decode as bytes,
        // so every textual candidate has to have been tried first.
        if let Ok(v) = self.try_get::<Vec<u8>, _>(ordinal) {
            return Ok(Value::Bytes(v));
        }

        Err(CursorError::RowValueUndecodable(label.to_owned()))
    }
}
