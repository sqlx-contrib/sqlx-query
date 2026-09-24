//! The clause types [`QueryComposer`](crate::QueryComposer)'s builder
//! methods accept: [`WhereClause`] (`push_where`), [`OrderByClause`]
//! (`push_order_by`), and [`Cursor`] (`with_cursor`, itself producing a
//! `WhereClause`/`OrderByClause` pair for keyset pagination, with [`Pager`]
//! cutting the fetched rows into a [`Page`] and the cursor to the next).

mod order;
mod pager;
// `where` is a reserved keyword, so this needs the raw-identifier form —
// only here and in the `pub use` below; everything else refers to the
// type via the crate-root re-export of `WhereClause`.
mod r#where;

pub use order::{OrderByClause, OrderByClauseError, OrderDirection};
pub use pager::{Cursor, CursorError, Page, Pager};
pub use r#where::WhereClause;
