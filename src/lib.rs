//! Adds filters and ordering to a SQL query you already wrote, by rewriting
//! its syntax tree, for [sqlx].
//!
//! ```
//! # #[cfg(feature = "postgres")] {
//! use sqlx::Postgres;
//! use sqlx_query::QueryWriter;
//!
//! // The query you already wrote. Nothing in it belongs to this crate -- it
//! // runs in psql, it EXPLAINs, and `sqlx::query!` will check it against a
//! // live database.
//! let mut writer = QueryWriter::<Postgres>::new(
//!     "SELECT id, title, read_count FROM volumes WHERE tenant_id = $1 ORDER BY id",
//! )?;
//!
//! // What the request asked for, as fragments. Each is parsed before it is
//! // used, so a fragment that is not one complete expression never lands in
//! // the query.
//! writer
//!     .bind(7_i64)
//!     .filter_by("read_count > 100")
//!     .order_by("title desc")
//!     .limit(50);
//!
//! assert_eq!(
//!     writer.sql()?,
//!     "SELECT id, title, read_count FROM volumes \
//!      WHERE tenant_id = $1 AND read_count > 100 \
//!      ORDER BY title DESC, id LIMIT 50",
//! );
//!
//! // let rows = writer.build_as::<Volume>()?.fetch_all(&pool).await?;
//! # }
//! # Ok::<_, sqlx_query::Error>(())
//! ```
//!
//! # Why a tree and not a template
//!
//! The query above has no holes, no markers and no escaping. That is the whole
//! point: a skeleton with `{}` in it is not SQL, so nothing that reads SQL can
//! read it -- not your formatter, not `EXPLAIN`, not the compile-time check in
//! `sqlx::query!`. Here the skeleton is the statement, and the parts that vary
//! are grafted onto its tree.
//!
//! It also means the rewrite knows what it is editing. Adding a filter to
//! `WHERE a = 1 OR b = 2` has to parenthesise the existing condition or
//! quietly change the query, because `AND` binds tighter than `OR`. A
//! rewriter that only has text cannot see that; one that has a tree cannot
//! miss it.
//!
//! # What a fragment is allowed to be
//!
//! Exactly one expression. [`filter_by`](QueryWriter::filter_by) parses its
//! argument and then insists the parser reached the end of it, so
//! `role = 'admin'` is accepted and `role = 'admin'; DROP TABLE users` is
//! [`Error::Trailing`] -- the statement after the expression has nowhere to go.
//!
//! That is a check on shape. It is not a claim that any expression is safe to
//! run: `role = 'admin' OR 1=1` is well-formed. Fragments should be built from
//! an allowlist of columns, with values bound rather than written in.
//!
//! # Placeholder numbering is not the same everywhere
//!
//! PostgreSQL's `$N` names the *N*th bound value, so a fragment spliced ahead
//! of a `$2` leaves it pointing at the same thing. MySQL's and SQLite's `?`
//! names the *N*th placeholder *in the text*, so splicing ahead of one shifts
//! what it binds. Where a rewrite would do that, this crate returns
//! [`Error::Positional`] rather than a query that runs and is wrong.
//!
//! [sqlx]: https://github.com/launchbadge/sqlx

#![cfg_attr(docsrs, feature(doc_cfg))]

#[cfg(not(any(feature = "postgres", feature = "mysql", feature = "sqlite")))]
compile_error!(
    "sqlx-query needs at least one driver feature: `postgres`, `mysql`, or `sqlite`. \
     Without one there is no dialect to parse with and no `Arguments` to bind against."
);

mod dialect;
mod error;
mod placeholder;
mod writer;

pub use dialect::Dialect;
pub use error::Error;
pub use writer::QueryWriter;
