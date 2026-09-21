//! Splices a [`WhereClause`] and an [`OrderByClause`] into
//! `/* query.<name> */` sentinel comments in a base SQL query the caller
//! already wrote, instead of building a `SELECT` from scratch. See
//! `DESIGN.md` in the repo root for the full rationale.

use std::collections::HashMap;

mod composer;
mod cursor;
mod dialect;
mod order_by;
mod shift;
mod value;
mod where_clause;

pub use composer::{Error, QueryComposer};
pub use cursor::{Cursor, CursorError};
pub use dialect::QueryDialect;
pub use order_by::{OrderByClause, OrderByClauseError, OrderDirection};
pub use value::Value;
pub use where_clause::WhereClause;

/// Renames the field names a fragment was parsed with to real column names,
/// against a fail-closed allow-list: any field not present as a key in
/// `columns` is an error, not passed through.
///
/// Kept separate from rendering a value to SQL text on purpose — resolving
/// and rendering are different steps, and not every value handed to
/// [`QueryComposer`] needs an allow-list (types that don't carry field
/// names simply don't implement this trait). Lives here rather than its
/// own file: unlike [`QueryDialect`], which has real per-dialect impls,
/// every `impl QueryResolver` lives with the implementing type, so this
/// file is the only place this trait's definition itself belongs.
pub trait QueryResolver: Sized {
    type Error;

    fn resolve(self, columns: &HashMap<&str, &str>) -> Result<Self, Self::Error>;
}
