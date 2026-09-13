//! The per-driver syntax this crate has to vary.

use chrono::{DateTime, Utc};
use sqlx::database::Database;
use sqlx::decode::Decode;
use sqlx::types::Type;
use sqlx::{Error as SqlxError, Row};

use crate::error::Error;
use crate::mapping::{Column, ColumnType};
use crate::value::Value;

/// The SQL spellings that differ between drivers.
///
/// # Implementing it for a driver this crate has not heard of
///
/// Deliberately not sealed. sqlx's [`Database`] is open, and a driver can live
/// outside sqlx entirely -- Cloudflare D1, `libSQL` and `DuckDB` are the sort that
/// arrive that way. Sealing this would have made the crate unusable with every
/// one of them, for the sake of a rule ("a driver's quote character is a fact,
/// not a preference") that only holds for drivers already supported. For one
/// that is not, there is no answer here to protect -- only a missing one.
///
/// Everything else was already open: [`Value`] implements `Encode` and `Type`
/// for any `Database` that handles the six primitives underneath, and
/// placeholders come from the driver itself.
///
/// Three items, none of them long:
///
/// Not compiled here, because an example cannot implement this crate's trait
/// for a driver it does not own -- which is the orphan rule doing its job, and
/// the reason this has to be your crate's code and not ours.
///
/// ```ignore
/// use sqlx_query::{ColumnType, Dialect, Error, Value, value_from_row};
///
/// impl Dialect for D1 {
///     const QUOTE: char = '"';
///
///     fn bind(
///         arguments: &mut Self::Arguments,
///         value: Value,
///     ) -> Result<(), sqlx::error::BoxDynError> {
///         use sqlx::Arguments as _;
///         arguments.add(value)
///     }
///
///     fn value(row: &Self::Row, column: &str, ty: ColumnType) -> Result<Value, Error> {
///         value_from_row(row, column, ty)
///     }
/// }
/// ```
///
/// [`value_from_row`] is the whole of [`value`](Self::value) for every driver
/// here, and is public for exactly this.
///
/// One limit, and it is Rust's rather than this crate's: the impl has to live
/// in the crate that defines the driver type. A third crate holding neither
/// `Dialect` nor the `Database` impl gets E0117 -- so bolting this onto a
/// driver you merely depend on means asking its author, or wrapping it in a
/// newtype that implements `Database` itself. Unsealing removes the barrier
/// this crate put up; it cannot remove that one.
///
/// The cost of being open: adding a method to this trait is a breaking change,
/// so it will not gain one lightly.
///
/// [`Value`]: crate::Value
/// [`value_from_row`]: crate::value_from_row
///
/// # Notably absent: anything about placeholders
///
/// `$1` versus `?` is [`Arguments::format_placeholder`]'s job, and every
/// fragment is rendered through it, so numbering and offsets cannot be got
/// wrong here. [`QueryBuilder`] asks the driver directly when it needs to know
/// whether placeholders are numbered.
///
/// [`Arguments::format_placeholder`]: sqlx::Arguments::format_placeholder
/// [`QueryBuilder`]: crate::QueryBuilder
pub trait Dialect: Database {
    /// The identifier quote character. Doubled to escape itself.
    const QUOTE: char;

    /// Append a [`Value`] to an argument list.
    ///
    /// The mirror of [`value`](Self::value), and here for the same reason: it
    /// keeps `Value: Encode<DB> + Type<DB>` inside the three driver impls,
    /// where it holds by inspection, rather than on every signature that binds
    /// one.
    ///
    /// # Errors
    ///
    /// Whatever the driver's encoder returns.
    fn bind(arguments: &mut Self::Arguments, value: Value) -> Result<(), sqlx::error::BoxDynError>;

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

/// Read a value of `ty` out of `row`'s `column`.
///
/// The body of [`Dialect::value`] for every driver in this crate, and public so
/// that it can be the body of yours: the bounds are what stop this living on
/// the trait, since a where-clause on a trait is an obligation at every use
/// site rather than an implied bound.
///
/// # Errors
///
/// [`Error::Column`] if the column is absent from the row, or its value does
/// not decode as `ty`.
pub fn value_from_row<R>(row: &R, column: &str, ty: ColumnType) -> Result<Value, Error>
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
#[cfg_attr(docsrs, doc(cfg(feature = "postgres")))]
impl Dialect for sqlx::Postgres {
    const QUOTE: char = '"';

    fn bind(arguments: &mut Self::Arguments, value: Value) -> Result<(), sqlx::error::BoxDynError> {
        use sqlx::Arguments as _;

        arguments.add(value)
    }

    fn value(row: &Self::Row, column: &str, ty: ColumnType) -> Result<Value, Error> {
        value_from_row(row, column, ty)
    }
}

#[cfg(feature = "sqlite")]
#[cfg_attr(docsrs, doc(cfg(feature = "sqlite")))]
impl Dialect for sqlx::Sqlite {
    const QUOTE: char = '"';

    fn bind(arguments: &mut Self::Arguments, value: Value) -> Result<(), sqlx::error::BoxDynError> {
        use sqlx::Arguments as _;

        arguments.add(value)
    }

    fn value(row: &Self::Row, column: &str, ty: ColumnType) -> Result<Value, Error> {
        value_from_row(row, column, ty)
    }
}

#[cfg(feature = "mysql")]
#[cfg_attr(docsrs, doc(cfg(feature = "mysql")))]
impl Dialect for sqlx::MySql {
    const QUOTE: char = '`';

    fn bind(arguments: &mut Self::Arguments, value: Value) -> Result<(), sqlx::error::BoxDynError> {
        use sqlx::Arguments as _;

        arguments.add(value)
    }

    fn value(row: &Self::Row, column: &str, ty: ColumnType) -> Result<Value, Error> {
        value_from_row(row, column, ty)
    }
}

/// Write `name` as a quoted identifier.
///
/// Column names reach here from a [`Mapping`](crate::Mapping), never from a
/// request -- but quoting is what makes that separation hold even if a mapping
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

/// Write a column as a quoted, possibly qualified reference.
///
/// `"a"."name"` when qualified, `"name"` when not. Each part is quoted on its
/// own, so a qualifier that came from configuration is escaped rather than
/// splitting the identifier.
pub(crate) fn reference<DB: Dialect>(column: &Column) -> String {
    let mut out = String::with_capacity(column.name.len() + 2);

    if let Some(qualifier) = &column.qualifier {
        quote::<DB>(qualifier, &mut out);
        out.push('.');
    }

    quote::<DB>(&column.name, &mut out);
    out
}

#[cfg(test)]
mod tests {
    // Every test here renders against a concrete driver.
    #[cfg(any(feature = "postgres", feature = "mysql"))]
    use super::*;
    #[cfg(any(feature = "postgres", feature = "mysql"))]
    use crate::mapping::ColumnType;

    /// A join needs `"a"."name"`, not one identifier containing a dot.
    #[cfg(feature = "postgres")]
    #[test]
    fn a_qualified_column_quotes_each_part() {
        use sqlx::Postgres;

        let column = Column::new("name", ColumnType::Text).with_qualifier("a");

        assert_eq!(reference::<Postgres>(&column), r#""a"."name""#);
    }

    #[cfg(feature = "postgres")]
    #[test]
    fn a_qualifier_from_configuration_is_escaped_too() {
        use sqlx::Postgres;

        let column = Column::new("name", ColumnType::Text).with_qualifier(r#"a"."b"#);

        assert_eq!(reference::<Postgres>(&column), r#""a"".""b"."name""#);
    }

    #[cfg(feature = "postgres")]
    #[test]
    fn postgres_quotes_with_double_quotes() {
        use sqlx::Postgres;

        assert_eq!(
            reference::<Postgres>(&Column::new("read_count", ColumnType::Text)),
            r#""read_count""#
        );
    }

    #[cfg(feature = "mysql")]
    #[test]
    fn mysql_quotes_with_backticks() {
        use sqlx::MySql;

        assert_eq!(
            reference::<MySql>(&Column::new("read_count", ColumnType::Text)),
            "`read_count`"
        );
    }

    /// A mapping built from configuration could carry anything. Doubling the
    /// quote keeps it an identifier instead of an injection point -- and the
    /// qualifier is quoted on its own, so it cannot split the identifier
    /// either.
    #[cfg(feature = "postgres")]
    #[test]
    fn a_quote_inside_a_name_is_escaped_not_honoured() {
        use sqlx::Postgres;

        let column = Column::new(r#"a" OR 1=1 --"#, ColumnType::Text);

        assert_eq!(reference::<Postgres>(&column), r#""a"" OR 1=1 --""#);
    }

    #[cfg(feature = "mysql")]
    #[test]
    fn a_backtick_inside_a_name_is_escaped_too() {
        use sqlx::MySql;

        assert_eq!(
            reference::<MySql>(&Column::new("a`b", ColumnType::Text)),
            "`a``b`"
        );
    }
}
