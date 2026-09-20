//! Splices [`QueryFragment`]s (a `WHERE`-clause filter, an `ORDER BY`
//! clause, ...) into `/* query.<name> */` sentinel comments in a base SQL
//! query the caller already wrote, instead of building a `SELECT` from
//! scratch. See `DESIGN.md` in the repo root for the full rationale.

mod composer;
mod dialect;
mod fragment;
mod order_by;
mod resolver;
mod shift;
mod value;

pub use composer::{Error, QueryComposer};
pub use dialect::QueryDialect;
pub use fragment::QueryFragment;
pub use order_by::{Direction, OrderBy, OrderByError};
pub use resolver::QueryResolver;
pub use value::Value;
