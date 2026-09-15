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
//!     .filter("read_count > 100")
//!     .sort("title desc")
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
//! # Two kinds of argument
//!
//! [`filter`](QueryWriter::filter) and [`sort`](QueryWriter::sort) each take
//! either a SQL fragment, which you wrote and vouch for, or something a client
//! asked for that was checked against a map of the fields you chose to offer.
//! The call site shows which.
//!
//! ```
//! # #[cfg(feature = "postgres")] {
//! # use std::collections::HashMap;
//! use sqlx::Postgres;
//! use sqlx_query::{QueryWriter, Sort};
//!
//! // What a request may order by, and the column each name means. Anything
//! // not here is refused rather than passed through.
//! let columns = HashMap::from([("title", "title"), ("id", "id")]);
//! let sort = Sort::parse("title desc")?.asc("id").resolve(&columns)?;
//!
//! let mut writer = QueryWriter::<Postgres>::new(
//!     "SELECT id, title FROM volumes WHERE tenant_id = $1",
//! )?;
//!
//! writer
//!     .bind(7_i64)
//!     .filter("visible")   // a fragment: yours
//!     .sort(&sort)         // a request: checked
//!     .limit(50);
//!
//! assert_eq!(
//!     writer.sql()?,
//!     "SELECT id, title FROM volumes WHERE tenant_id = $1 AND visible \
//!      ORDER BY \"title\" DESC, \"id\" ASC LIMIT 50",
//! );
//! # }
//! # Ok::<_, sqlx_query::Error>(())
//! ```
//!
//! A [`Sort`] that was never resolved is refused rather than written into the
//! query, so forgetting the step cannot quietly skip the allowlist.
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
//! Exactly one expression. [`filter`](QueryWriter::filter) parses its
//! argument and then insists the parser reached the end of it, so
//! `role = 'admin'` is accepted and `role = 'admin'; DROP TABLE users` is
//! [`Error::Trailing`] -- the statement after the expression has nowhere to go.
//!
//! That is a check on shape. It is not a claim that any expression is safe to
//! run: `role = 'admin' OR 1=1` is well-formed. Fragments should be built from
//! an allowlist of columns, with values bound rather than written in.
//!
//! # Placeholders are numbered, then written back out
//!
//! Whatever the driver spells them as, placeholders are numbered on the way in
//! and written back in that driver's form at the end. In between there is one
//! kind of placeholder and the rewrite is arithmetic: a fragment's `$1` becomes
//! `$3` because two values were claimed before it.
//!
//! Values are bound in the order the placeholders claim them -- the base
//! query's first, then each fragment's -- and sent in that same order, because
//! every placeholder names the value it wants rather than merely occupying a
//! position:
//!
//! ```text
//! base      SELECT id FROM users WHERE tenant_id = ? LIMIT ?
//! fragment  role = ?
//! sqlite    SELECT id FROM users WHERE tenant_id = ?1 AND role = ?3 LIMIT ?2
//! postgres  SELECT id FROM users WHERE tenant_id = $1 AND role = $3 LIMIT $2
//! ```
//!
//! Naming is why both drivers here are supported and MySQL is not. Its `?`
//! takes a value per appearance and cannot ask for an earlier one, so the same
//! rewrite would have to reorder the values to match -- silently, since the SQL
//! would look identical either way. Supporting it later is possible; doing it
//! quietly is not.
//!
//! A base query uses its own driver's syntax, because it is SQL for that
//! database and nothing else. PostgreSQL will not parse `?`, and SQLite takes
//! `?`, `?N` or `$N`.
//!
//! [sqlx]: https://github.com/launchbadge/sqlx

#![cfg_attr(docsrs, feature(doc_cfg))]

#[cfg(not(any(feature = "postgres", feature = "sqlite")))]
compile_error!(
    "sqlx-query needs at least one driver feature: `postgres` or `sqlite`. \
     Without one there is no syntax to parse with and no `Arguments` to bind against."
);

mod syntax;
mod writer;

#[cfg(feature = "cel")]
#[cfg_attr(docsrs, doc(cfg(feature = "cel")))]
pub use syntax::Filter;
pub use syntax::{
    Error, FilterExpr, IntoFilterExpr, IntoSortExpr, Literal, Sort, SortDirection, SortExpr,
    SortKey, Syntax,
};
pub use writer::QueryWriter;
