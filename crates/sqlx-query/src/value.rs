use chrono::{DateTime, Utc};

/// A dialect-agnostic bind value. [`QueryFragment`](crate::QueryFragment)
/// implementors produce these instead of binding directly against a
/// concrete `sqlx::Database`, so a single fragment (e.g. a CEL `Filter`)
/// can be spliced into a query for any dialect `QueryComposer` supports.
#[derive(Debug, Clone, PartialEq)]
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
