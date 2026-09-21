//! Splices a [`WhereClause`] and an [`OrderByClause`] into
//! `/* query.<name> */` sentinel comments in a base SQL query the caller
//! already wrote, instead of building a `SELECT` from scratch — a port of
//! `pgx-contrib/pgxquery`'s sentinel-comment splicing technique to
//! Rust/sqlx.

use std::collections::HashMap;

mod clauses;
mod composer;
mod dialect;
mod lexer;
mod value;

pub use clauses::{
    Cursor, CursorError, OrderByClause, OrderByClauseError, OrderDirection, WhereClause,
};
pub use composer::{Error, QueryComposer, QueryStatement};
pub use dialect::QueryDialect;
pub use value::Value;

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

    /// # Errors
    ///
    /// Returns `Self::Error` for any field name `columns` doesn't have a
    /// column for — the fail-closed half of the allow-list.
    fn resolve(self, columns: &HashMap<&str, &str>) -> Result<Self, Self::Error>;
}
