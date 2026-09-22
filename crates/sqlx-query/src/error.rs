use crate::lexer::PlaceholderError;
use crate::{CursorError, OrderByClauseError, QueryComposerError};

/// Everything this crate can fail at, one variant per domain that owns a
/// failure of its own.
///
/// Each variant forwards to the error the failing type defines, rather
/// than restating it: a cursor's problems are described by
/// [`CursorError`] wherever they surface, so the same fault reads the same
/// whether it came out of [`Cursor::parse`](crate::Cursor::parse) or out
/// of [`QueryComposer::compose`](crate::QueryComposer::compose).
///
/// Every variant is `#[from]`, so a layer above can `?` a parse, a
/// resolve and a compose into this one type. Not all of them can come out
/// of `compose` — [`OrderBy`](Self::OrderBy) is raised while parsing,
/// before a composer exists — which is what makes this the crate's
/// aggregate rather than the composer's error.
///
/// Ordered by descending scope: the operation the crate exists to perform,
/// then the two clause domains that feed it, then the lexical detail
/// underneath them all.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Splicing failed: the query and the values, or the query and the
    /// clauses, disagree.
    #[error(transparent)]
    Composer(#[from] QueryComposerError),

    /// A cursor couldn't be built, decoded, or applied.
    #[error(transparent)]
    Cursor(#[from] CursorError),

    /// An `order_by` couldn't be parsed or resolved.
    #[error(transparent)]
    OrderBy(#[from] OrderByClauseError),

    /// A placeholder spelled in a way the dialect doesn't use.
    #[error(transparent)]
    Placeholder(#[from] PlaceholderError),
}
