//! Splices SQL fragments into the sentinel comments of a query you already
//! wrote, for [sqlx].
//!
//! ```
//! # #[cfg(feature = "postgres")] {
//! use sqlx::Postgres;
//! use sqlx_query::{QueryFragment, QueryTemplate};
//!
//! let template = QueryTemplate::<Postgres>::parse(
//!     "SELECT id, title FROM volumes
//!       WHERE tenant_id = $1
//!         /* AND query.predicate */
//!       /* ORDER BY query.order */
//!       LIMIT $2",
//! )?;
//!
//! let mut recent = QueryFragment::<Postgres, i64>::new();
//! recent.push("read_count > ").push_bind(100);
//!
//! let mut order = QueryFragment::<Postgres, i64>::new();
//! order.push("\"title\" ASC, \"id\" ASC");
//!
//! let query = template
//!     .splice()
//!     .bind(7_i64)   // $1
//!     .bind(50_i64)  // $2
//!     .fill("predicate", &recent)
//!     .fill("order", &order);
//!
//! assert!(query.sql().contains("AND read_count > $3"));
//! # query.build()?;
//! # }
//! # Ok::<_, sqlx_query::Error>(())
//! ```
//!
//! # What the sentinels buy
//!
//! The skeleton above is a statement. Comments are inert, so you can paste it
//! into `psql`, `EXPLAIN` it, or hand
//! [`skeleton()`](QueryTemplate::skeleton) to `sqlx::query!` and have the
//! database check it at compile time. A template language with `{}` holes
//! cannot do any of that, because its skeleton is not SQL.
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
//! # Placeholder numbering is not the same everywhere
//!
//! PostgreSQL's `$N` names the *N*th bound value, so text spliced ahead of a
//! `$2` leaves it alone. MySQL's and SQLite's `?` names the *N*th placeholder
//! *in the text*, so splicing ahead of one shifts it. Where that distinction
//! bites -- binding after filling, or filling slots out of order -- this crate
//! returns [`Error::Positional`] rather than a wrong answer. See [`Splice`].
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
mod splice;
mod template;

pub use error::Error;
pub use fragment::QueryFragment;
pub use splice::{Slot, Splice};
pub use template::QueryTemplate;
