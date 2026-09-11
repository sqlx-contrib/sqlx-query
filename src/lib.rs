//! Splices SQL fragments into the sentinel comments of a query you already
//! wrote, for [sqlx].
//!
//! # What the fragments buy
//!
//! A [`QueryFragment`] stores the SQL *between* its binds and leaves the
//! placeholders to be written at splice time, by the driver, through
//! [`Arguments::format_placeholder`]. So `$3` is produced once, when the value
//! is added -- there is no pass that rewrites `?` into `$3` afterwards, and so
//! no chance of a rewrite wandering into a string literal.
//!
//! It also means a fragment carries no driver bounds until it is used: build
//! one anywhere, encode it when it lands in a query.
//!
//! [sqlx]: https://github.com/launchbadge/sqlx
//! [`Arguments::format_placeholder`]: sqlx::Arguments::format_placeholder

#![cfg_attr(docsrs, feature(doc_cfg))]

#[cfg(not(any(feature = "postgres", feature = "mysql", feature = "sqlite")))]
compile_error!(
    "sqlx-query needs at least one driver feature: `postgres`, `mysql`, or `sqlite`. \
     Without one there is no `Arguments` implementation to splice against."
);

mod error;
mod fragment;

pub use error::Error;
pub use fragment::QueryFragment;
