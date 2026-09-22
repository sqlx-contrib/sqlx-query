use crate::lexer::PlaceholderError;
use crate::{CursorError, QueryComposerError};

/// Everything composing can fail at, one variant per domain that owns a
/// failure of its own.
///
/// Each variant forwards to the error the failing type defines, rather
/// than restating it: a cursor's problems are described by
/// [`CursorError`] wherever they surface, so the same fault reads the same
/// whether it came out of [`Cursor::parse`](crate::Cursor::parse) or out
/// of [`QueryComposer::compose`](crate::QueryComposer::compose).
///
/// Every variant is `#[from]`, and every one of them can actually come out
/// of [`compose`](crate::QueryComposer::compose) — a caller matching
/// exhaustively is never handling a case this crate can't produce.
/// [`OrderByClauseError`](crate::OrderByClauseError) is deliberately
/// absent: parsing and resolving an `order_by` happen before a composer
/// exists, so a caller who wants one type for both stages builds that
/// aggregate themselves.
///
/// Ordered by descending scope: the operation the crate exists to perform,
/// then the clause domain that feeds it, then the lexical detail
/// underneath them both.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Splicing failed: the query and the values, or the query and the
    /// clauses, disagree.
    #[error(transparent)]
    Composer(#[from] QueryComposerError),

    /// A cursor couldn't be built, decoded, or applied.
    #[error(transparent)]
    Cursor(#[from] CursorError),

    /// A placeholder spelled in a way the dialect doesn't use.
    #[error(transparent)]
    Placeholder(#[from] PlaceholderError),
}
