use std::error::Error as StdError;
use std::fmt;

use sqlparser::parser::ParserError;

/// What can go wrong between a query you wrote and the one that runs.
///
/// Every variant is raised before the database is touched: a rewrite either
/// produces a statement this crate is willing to vouch for, or it produces
/// this.
// `Clone` so that a rewrite which failed while the chain was still being built
// can report the same failure from every later `sql()` or `build()`, rather
// than reporting it once and then appearing to succeed.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum Error {
    /// The base query did not parse.
    Query(ParserError),

    /// A fragment did not parse as SQL.
    ///
    /// The fragment is carried along because the caller usually did not write
    /// it by hand -- it arrived from a filter compiler, and the text is the
    /// only way to see what that compiler emitted.
    Fragment {
        /// The fragment as given.
        fragment: String,
        /// Why the parser rejected it.
        source: ParserError,
    },

    /// A fragment parsed, but only a prefix of it was an expression.
    ///
    /// This is the variant that makes fragments safe to accept as text.
    /// `role = 'admin'` parses and consumes everything; `role = 'admin';
    /// DROP TABLE users` parses an expression and leaves a statement behind,
    /// and that leftover is refused here rather than spliced.
    Trailing {
        /// The fragment as given.
        fragment: String,
        /// The first token that was not part of the expression.
        rest: String,
    },

    /// The base SQL was not a query, so there is no `WHERE` to add to.
    NotQuery,

    /// The base query's outermost level is a `UNION`, `INTERSECT` or `EXCEPT`.
    ///
    /// There is no single `SELECT` to attach a filter to, and picking one of
    /// the branches would silently filter half the result. Wrap the set
    /// operation in an outer `SELECT ... FROM (...) AS t` and rewrite that.
    SetOperation,

    /// The base query has a `GROUP BY`, so a filter is ambiguous.
    ///
    /// A predicate over a grouping column belongs in `WHERE`, one over an
    /// aggregate belongs in `HAVING`, and the two run at different times
    /// against different rows. Nothing in the fragment says which was meant.
    Grouped,

    /// A bound value could not be encoded for this driver.
    ///
    /// Carried as text because sqlx's own encode error is not `Clone`, and
    /// this one has to survive being reported from more than one call.
    Encode(String),

    /// The rewrite removed a placeholder, leaving a value with nothing to bind
    /// to.
    ///
    /// [`limit`](crate::QueryWriter::limit) replaces the query's own `LIMIT`,
    /// so a base query that said `LIMIT $2` loses `$2` -- and the driver would
    /// then be handed one more value than the statement has places for. Take
    /// the `LIMIT` out of the base query, or keep it and do not call `limit`.
    Orphaned,

    /// Fewer values were bound than the statement has placeholders.
    Unbound {
        /// How many the statement asks for.
        wanted: usize,
        /// How many were given.
        given: usize,
    },

    /// One value is wanted by two placeholders, which `?` cannot express.
    ///
    /// `$1` twice is ordinary, and PostgreSQL binds one value to both. `?`
    /// takes a value per placeholder and has no way to name an earlier one, so
    /// there is nothing to render this as.
    ///
    /// Reordering, which this used to mean, is no longer an error: values are
    /// replayed in the order the finished statement asks for.
    Positional,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Query(source) => write!(f, "the query did not parse: {source}"),
            Self::Fragment { fragment, source } => {
                write!(f, "the fragment `{fragment}` did not parse: {source}")
            }
            Self::Trailing { fragment, rest } => write!(
                f,
                "the fragment `{fragment}` is an expression followed by `{rest}`; \
                 a fragment has to be one complete expression and nothing else",
            ),
            Self::NotQuery => f.write_str("the SQL is not a query, so it has no WHERE to add to"),
            Self::SetOperation => f.write_str(
                "the query's outermost level is a set operation, which has no single SELECT \
                 to filter; wrap it in `SELECT * FROM (...) AS t` and rewrite that instead",
            ),
            Self::Encode(message) => write!(f, "a bound value could not be encoded: {message}"),
            Self::Orphaned => f.write_str(
                "this rewrite removed a placeholder the base query had, which would leave a \
                 bound value with nothing to bind to; if the base query ends in `LIMIT $n`, \
                 either drop it or do not call `limit()`",
            ),
            Self::Grouped => f.write_str(
                "the query has a GROUP BY, so a filter could mean WHERE or HAVING; \
                 put the predicate in the query itself",
            ),
            Self::Unbound { wanted, given } => write!(
                f,
                "the statement has {wanted} placeholders but {given} values were bound",
            ),
            Self::Positional => f.write_str(
                "one value is wanted by two placeholders, and this driver's `?` takes a value \
                 per placeholder with no way to name an earlier one; bind it twice, or use \
                 PostgreSQL, whose `$1` can repeat",
            ),
        }
    }
}

impl StdError for Error {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        match self {
            Self::Query(source) | Self::Fragment { source, .. } => Some(source),
            _ => None,
        }
    }
}
