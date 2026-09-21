//! Splices a [`Where`] clause and an [`OrderBy`] clause into
//! `/* query.<name> */` sentinel comments in a base SQL query the caller
//! already wrote, instead of building a `SELECT` from scratch. See
//! `DESIGN.md` in the repo root for the full rationale.

mod composer;
mod dialect;
mod order_by;
mod resolver;
mod shift;
mod value;
mod where_clause;

pub use composer::{Error, QueryComposer};
pub use dialect::QueryDialect;
pub use order_by::{Direction, OrderBy, OrderByError};
pub use resolver::QueryResolver;
pub use value::Value;
pub use where_clause::Where;
