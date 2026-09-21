//! The clause types [`QueryComposer`](crate::QueryComposer)'s builder
//! methods accept: [`WhereClause`] (`where_by`), [`OrderClause`]
//! (`order_by`), and [`Cursor`] (`cursor`, itself producing a
//! `WhereClause`/`OrderClause` pair for keyset pagination).

mod order;
mod pager;
// `where` is a reserved keyword, so this needs the raw-identifier form —
// only here and in the `pub use` below; everything else refers to the
// type via the crate-root re-export of `WhereClause`.
mod r#where;

pub use order::{OrderClause, OrderClauseError, OrderDirection};
pub use pager::{Cursor, CursorError};
pub use r#where::WhereClause;
