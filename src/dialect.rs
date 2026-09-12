//! The per-driver syntax this crate has to vary.

use chrono::{DateTime, Utc};
use sqlx::database::Database;
use sqlx::decode::Decode;
use sqlx::types::Type;
use sqlx::{Error as SqlxError, Row};

use crate::error::Error;
use crate::schema::ColumnType;
use crate::value::Value;

mod sealed {
    pub trait Sealed {}
}

/// The SQL spellings that differ between drivers.
///
/// Sealed: the constants here are facts about a driver, not policy, so there is
/// no sensible way for a caller to supply a different answer for one this crate
/// already supports.
///
/// # Notably absent: anything about placeholders
///
/// `$1` versus `?` is [`Arguments::format_placeholder`]'s job, and every
/// fragment is rendered through it, so numbering and offsets cannot be got
/// wrong here. [`Splice`] asks the driver directly when it needs to know
/// whether placeholders are numbered.
///
/// [`Arguments::format_placeholder`]: sqlx::Arguments::format_placeholder
/// [`Splice`]: crate::Splice
pub trait Dialect: Database + sealed::Sealed {
    /// The identifier quote character. Doubled to escape itself.
    const QUOTE: char;

    /// Read a value of `ty` out of `row`'s `column`.
    ///
    /// Exists so that the six `Decode` bounds one per [`Value`] variant stay
    /// inside the three driver impls, where they are satisfied by inspection,
    /// rather than appearing on every signature that wants to read a row. A
    /// where-clause on a trait is not an implied bound for that trait's users --
    /// it becomes an obligation at each use site.
    ///
    /// # Errors
    ///
    /// [`Error::Column`] if the column is absent from the row, or its value
    /// does not decode as `ty`.
    fn value(row: &Self::Row, column: &str, ty: ColumnType) -> Result<Value, Error>;
}

/// One implementation for every driver, reached through [`Dialect::value`].
fn value_from_row<R>(row: &R, column: &str, ty: ColumnType) -> Result<Value, Error>
where
    R: Row,
    // Column lookup by name is per-driver, not blanket.
    for<'a> &'a str: sqlx::ColumnIndex<R>,
    bool: for<'r> Decode<'r, R::Database> + Type<R::Database>,
    i64: for<'r> Decode<'r, R::Database> + Type<R::Database>,
    f64: for<'r> Decode<'r, R::Database> + Type<R::Database>,
    String: for<'r> Decode<'r, R::Database> + Type<R::Database>,
    Vec<u8>: for<'r> Decode<'r, R::Database> + Type<R::Database>,
    DateTime<Utc>: for<'r> Decode<'r, R::Database> + Type<R::Database>,
{
    fn explain(column: &str, error: &SqlxError) -> Error {
        Error::Column(match error {
            SqlxError::ColumnNotFound(_) => format!(
                "`{column}` is not in the row: a sort key's column has to be in \
                 the SELECT list for a page token to be built from it"
            ),
            other => format!("`{column}`: {other}"),
        })
    }

    Ok(match ty {
        ColumnType::Bool => Value::Bool(row.try_get(column).map_err(|e| explain(column, &e))?),
        ColumnType::Int => Value::Int(row.try_get(column).map_err(|e| explain(column, &e))?),
        ColumnType::Float => Value::Float(row.try_get(column).map_err(|e| explain(column, &e))?),
        ColumnType::Text => Value::Text(row.try_get(column).map_err(|e| explain(column, &e))?),
        ColumnType::Bytes => Value::Bytes(row.try_get(column).map_err(|e| explain(column, &e))?),
        ColumnType::Timestamp => {
            Value::Timestamp(row.try_get(column).map_err(|e| explain(column, &e))?)
        }
    })
}

#[cfg(feature = "postgres")]
impl sealed::Sealed for sqlx::Postgres {}

#[cfg(feature = "postgres")]
#[cfg_attr(docsrs, doc(cfg(feature = "postgres")))]
impl Dialect for sqlx::Postgres {
    const QUOTE: char = '"';

    fn value(row: &Self::Row, column: &str, ty: ColumnType) -> Result<Value, Error> {
        value_from_row(row, column, ty)
    }
}

#[cfg(feature = "sqlite")]
impl sealed::Sealed for sqlx::Sqlite {}

#[cfg(feature = "sqlite")]
#[cfg_attr(docsrs, doc(cfg(feature = "sqlite")))]
impl Dialect for sqlx::Sqlite {
    const QUOTE: char = '"';

    fn value(row: &Self::Row, column: &str, ty: ColumnType) -> Result<Value, Error> {
        value_from_row(row, column, ty)
    }
}

#[cfg(feature = "mysql")]
impl sealed::Sealed for sqlx::MySql {}

#[cfg(feature = "mysql")]
#[cfg_attr(docsrs, doc(cfg(feature = "mysql")))]
impl Dialect for sqlx::MySql {
    const QUOTE: char = '`';

    fn value(row: &Self::Row, column: &str, ty: ColumnType) -> Result<Value, Error> {
        value_from_row(row, column, ty)
    }
}

/// Write `name` as a quoted identifier.
///
/// Column names reach here from a [`Schema`](crate::Schema), never from a
/// request -- but quoting is what makes that separation hold even if a schema
/// is built from configuration, and doubling the quote character means a name
/// containing one is escaped rather than ending the identifier early.
pub(crate) fn quote<DB: Dialect>(name: &str, out: &mut String) {
    out.push(DB::QUOTE);

    for character in name.chars() {
        if character == DB::QUOTE {
            out.push(DB::QUOTE);
        }
        out.push(character);
    }

    out.push(DB::QUOTE);
}

/// Write `name` as a quoted identifier, returning it.
pub(crate) fn quoted<DB: Dialect>(name: &str) -> String {
    let mut out = String::with_capacity(name.len() + 2);
    quote::<DB>(name, &mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(feature = "postgres")]
    #[test]
    fn postgres_quotes_with_double_quotes() {
        use sqlx::Postgres;

        assert_eq!(quoted::<Postgres>("read_count"), r#""read_count""#);
    }

    #[cfg(feature = "mysql")]
    #[test]
    fn mysql_quotes_with_backticks() {
        use sqlx::MySql;

        assert_eq!(quoted::<MySql>("read_count"), "`read_count`");
    }

    /// A schema built from configuration could carry anything. Doubling the
    /// quote keeps it an identifier instead of an injection point.
    #[cfg(feature = "postgres")]
    #[test]
    fn a_quote_inside_a_name_is_escaped_not_honoured() {
        use sqlx::Postgres;

        assert_eq!(quoted::<Postgres>(r#"a" OR 1=1 --"#), r#""a"" OR 1=1 --""#);
    }

    #[cfg(feature = "mysql")]
    #[test]
    fn a_backtick_inside_a_name_is_escaped_too() {
        use sqlx::MySql;

        assert_eq!(quoted::<MySql>("a`b"), "`a``b`");
    }
}
