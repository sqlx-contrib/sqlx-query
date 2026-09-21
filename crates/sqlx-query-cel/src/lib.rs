//! [`FilterClause`] (CEL -> `WHERE` fragment) implements
//! [`sqlx_query::QueryResolver`] the same way
//! [`sqlx_query::OrderByClause`] does, and `impl From<FilterClause> for
//! sqlx_query::WhereClause` so it plugs into
//! `QueryComposer::push_where`. See `filter` module docs for the
//! parse -> resolve -> render pipeline.

mod filter;

pub use filter::{FilterClause, FilterClauseError};
