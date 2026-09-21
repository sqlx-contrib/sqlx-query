use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::{Column, ColumnIndex, Decode, Row, Type, ValueRef};

use crate::CursorError;

/// A dialect-agnostic bind value. [`QueryFragment`](crate::QueryFragment)
/// implementors produce these instead of binding directly against a
/// concrete `sqlx::Database`, so a single fragment (e.g. a CEL `Filter`)
/// can be spliced into a query for any dialect `QueryComposer` supports.
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
                    Value::Int(value as i64)
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
                    Value::Float(value as f64)
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

impl Value {
    /// Reads one column's value off `row`, for
    /// [`Cursor::after_row`](crate::Cursor::after_row).
    ///
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
    pub(crate) fn from_row<'r, R>(row: &'r R, column: &str) -> Result<Value, CursorError>
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
        DateTime<Utc>: Decode<'r, R::Database> + Type<R::Database>,
    {
        let label = column.rsplit('.').next().unwrap_or(column);
        let ordinal = row
            .columns()
            .iter()
            .find(|c| c.name() == label)
            .map(Column::ordinal)
            .ok_or_else(|| CursorError::RowColumnMissing(label.to_owned()))?;

        let is_null = row
            .try_get_raw(ordinal)
            .map_err(|_| CursorError::RowColumnMissing(label.to_owned()))?
            .is_null();
        if is_null {
            return Ok(Value::Null);
        }

        if let Ok(v) = row.try_get::<i64, _>(ordinal) {
            return Ok(Value::Int(v));
        }
        if let Ok(v) = row.try_get::<i32, _>(ordinal) {
            return Ok(Value::Int(v.into()));
        }
        if let Ok(v) = row.try_get::<i16, _>(ordinal) {
            return Ok(Value::Int(v.into()));
        }
        if let Ok(v) = row.try_get::<f64, _>(ordinal) {
            return Ok(Value::Float(v));
        }
        if let Ok(v) = row.try_get::<f32, _>(ordinal) {
            return Ok(Value::Float(v.into()));
        }
        if let Ok(v) = row.try_get::<bool, _>(ordinal) {
            return Ok(Value::Bool(v));
        }
        if let Ok(v) = row.try_get::<DateTime<Utc>, _>(ordinal) {
            return Ok(Value::Timestamp(v));
        }
        if let Ok(v) = row.try_get::<String, _>(ordinal) {
            return Ok(Value::String(v));
        }

        Err(CursorError::RowValueUndecodable(label.to_owned()))
    }
}
