//! The bind values a fragment can carry.

use chrono::{DateTime, Utc};
use sqlx::database::Database;
use sqlx::encode::{Encode, IsNull};
use sqlx::error::BoxDynError;
use sqlx::types::Type;

use crate::mapping::ColumnType;

/// A value produced while rendering a fragment.
///
/// # There is deliberately no `Null`
///
/// A bound `NULL` makes a comparison *unknown*, not true, so `x = NULL` matches
/// nothing however it is written. The forms that mean what a caller intends --
/// `IS NULL` and `IS NOT NULL` -- take no bind parameter at all, so the variant
/// would exist only to be encoded incorrectly.
///
/// # Closed on purpose
///
/// Producers in this crate emit `Value` so that a fragment can be built without
/// naming a driver, and so that a mapping can type-check a comparison before the
/// database sees it. A caller who wants to bind something else can build a
/// [`QueryFragment<DB, T>`](crate::QueryFragment) over their own type, or reach
/// for [`SlotBuilder::push_bind`](crate::SlotBuilder::push_bind).
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum Value {
    /// Bound as `bool`.
    Bool(bool),
    /// Bound as `i64`.
    Int(i64),
    /// Bound as `f64`.
    Float(f64),
    /// Bound as `String`.
    Text(String),
    /// Bound as `Vec<u8>`.
    Bytes(Vec<u8>),
    /// Bound as `DateTime<Utc>`. UTC is the one instant type all three drivers
    /// accept.
    Timestamp(DateTime<Utc>),
}

impl Value {
    /// The column type this value can be compared against.
    #[must_use]
    pub fn ty(&self) -> ColumnType {
        match self {
            Self::Bool(_) => ColumnType::Bool,
            Self::Int(_) => ColumnType::Int,
            Self::Float(_) => ColumnType::Float,
            Self::Text(_) => ColumnType::Text,
            Self::Bytes(_) => ColumnType::Bytes,
            Self::Timestamp(_) => ColumnType::Timestamp,
        }
    }
}

/// Written as a blanket impl rather than one per driver, because every variant
/// delegates to a primitive that sqlx already implements everywhere. A driver
/// this crate has never heard of gets `Value` support for free, so long as it
/// encodes the six types underneath.
impl<DB: Database> Type<DB> for Value
where
    bool: Type<DB>,
    i64: Type<DB>,
    f64: Type<DB>,
    String: Type<DB>,
    Vec<u8>: Type<DB>,
    DateTime<Utc>: Type<DB>,
{
    /// Never consulted in practice: [`Encode::produces`] answers first, and it
    /// answers for every variant. This is the fallback the trait requires.
    fn type_info() -> DB::TypeInfo {
        <String as Type<DB>>::type_info()
    }

    fn compatible(ty: &DB::TypeInfo) -> bool {
        <bool as Type<DB>>::compatible(ty)
            || <i64 as Type<DB>>::compatible(ty)
            || <f64 as Type<DB>>::compatible(ty)
            || <String as Type<DB>>::compatible(ty)
            || <Vec<u8> as Type<DB>>::compatible(ty)
            || <DateTime<Utc> as Type<DB>>::compatible(ty)
    }
}

impl<'q, DB: Database> Encode<'q, DB> for Value
where
    bool: Encode<'q, DB> + Type<DB>,
    i64: Encode<'q, DB> + Type<DB>,
    f64: Encode<'q, DB> + Type<DB>,
    String: Encode<'q, DB> + Type<DB>,
    Vec<u8>: Encode<'q, DB> + Type<DB>,
    DateTime<Utc>: Encode<'q, DB> + Type<DB>,
{
    fn encode_by_ref(&self, buf: &mut DB::ArgumentBuffer) -> Result<IsNull, BoxDynError> {
        match self {
            Self::Bool(value) => value.encode_by_ref(buf),
            Self::Int(value) => value.encode_by_ref(buf),
            Self::Float(value) => value.encode_by_ref(buf),
            Self::Text(value) => value.encode_by_ref(buf),
            Self::Bytes(value) => value.encode_by_ref(buf),
            Self::Timestamp(value) => value.encode_by_ref(buf),
        }
    }

    /// The variant decides the type, which is the hook that lets one Rust type
    /// stand for six SQL ones. `Arguments::add` prefers this over
    /// [`Type::type_info`] precisely so value-dependent types are possible.
    fn produces(&self) -> Option<DB::TypeInfo> {
        Some(match self {
            Self::Bool(_) => <bool as Type<DB>>::type_info(),
            Self::Int(_) => <i64 as Type<DB>>::type_info(),
            Self::Float(_) => <f64 as Type<DB>>::type_info(),
            Self::Text(_) => <String as Type<DB>>::type_info(),
            Self::Bytes(_) => <Vec<u8> as Type<DB>>::type_info(),
            Self::Timestamp(_) => <DateTime<Utc> as Type<DB>>::type_info(),
        })
    }

    fn size_hint(&self) -> usize {
        match self {
            Self::Bool(value) => Encode::<DB>::size_hint(value),
            Self::Int(value) => Encode::<DB>::size_hint(value),
            Self::Float(value) => Encode::<DB>::size_hint(value),
            Self::Text(value) => Encode::<DB>::size_hint(value),
            Self::Bytes(value) => Encode::<DB>::size_hint(value),
            Self::Timestamp(value) => Encode::<DB>::size_hint(value),
        }
    }
}

#[cfg(test)]
mod tests {
    use sqlx::{Arguments, Postgres, postgres::PgArguments};

    use super::*;

    #[test]
    fn every_variant_reports_its_column_type() {
        assert_eq!(Value::Bool(true).ty(), ColumnType::Bool);
        assert_eq!(Value::Int(1).ty(), ColumnType::Int);
        assert_eq!(Value::Float(1.0).ty(), ColumnType::Float);
        assert_eq!(Value::Text("x".into()).ty(), ColumnType::Text);
        assert_eq!(Value::Bytes(vec![1]).ty(), ColumnType::Bytes);
        assert_eq!(Value::Timestamp(Utc::now()).ty(), ColumnType::Timestamp);
    }

    /// The point of `produces`: one Rust type, six SQL ones, chosen per value.
    #[test]
    fn a_value_encodes_as_the_type_its_variant_names() {
        let mut arguments = PgArguments::default();

        for value in [
            Value::Bool(true),
            Value::Int(1),
            Value::Float(1.0),
            Value::Text("x".into()),
            Value::Bytes(vec![1]),
            Value::Timestamp(Utc::now()),
        ] {
            let produced = Encode::<Postgres>::produces(&value);
            assert!(produced.is_some(), "{value:?} produced no type");
            arguments.add(&value).unwrap();
        }

        assert_eq!(arguments.len(), 6);
    }
}
