//! Splices SQL fragments into the sentinel comments of a query you already
//! wrote, for [sqlx].
//!
//! [sqlx]: https://github.com/launchbadge/sqlx

#![cfg_attr(docsrs, feature(doc_cfg))]

#[cfg(not(any(feature = "postgres", feature = "mysql", feature = "sqlite")))]
compile_error!(
    "sqlx-query needs at least one driver feature: `postgres`, `mysql`, or `sqlite`. \
     Without one there is no `Arguments` implementation to splice against."
);
