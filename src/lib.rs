//! Splices SQL fragments into the sentinel comments of a query you already
//! wrote, for [sqlx].
//!
//! ```
//! # #[cfg(all(feature = "postgres", feature = "cel"))] {
//! use sqlx::Postgres;
//! use sqlx_query::{Column, ColumnType, Cursor, Filter, QueryTemplate, Sort, Table};
//!
//! // The query you already wrote. The sentinels are comments, so this is a
//! // statement: it runs in psql, it EXPLAINs, and `skeleton()` hands it to
//! // `sqlx::query!` to be checked against a live database.
//! let volumes = QueryTemplate::<Postgres>::parse(
//!     "SELECT id, title, read_count FROM volumes \
//!      WHERE tenant_id = $1 /* AND query.predicate */ \
//!      /* ORDER BY query.order */ LIMIT $2",
//! )?;
//!
//! // The allow-list. A field not named here is rejected, not passed through.
//! let schema = Table::new()
//!     .key("id", ColumnType::Int)
//!     .column("title", ColumnType::Text)
//!     .add("readCount", Column::new("read_count", ColumnType::Int));
//!
//! // Request parameters, as the strings they arrive as. Each treats an empty
//! // string as "not asked for" rather than as an error.
//! let filter = Filter::parse("readCount > 100 && title.startsWith(\'D\')")?;
//! let sort = Sort::parse("title desc")?.asc("id");
//!
//! // Refused if this token was issued under a different ordering.
//! let cursor = Cursor::parse("")?;
//! cursor.validate(&sort)?;
//!
//! let query = volumes
//!     .splice()
//!     .bind(7_i64)   // $1, the tenant
//!     .bind(50_i64)  // $2, the page size
//!     .fill("predicate", &filter.to_fragment(&schema)?)
//!     .fill("predicate", &cursor.to_fragment(&schema)?)
//!     .fill("order", &sort.to_fragment(&schema)?);
//!
//! assert_eq!(
//!     query.sql(),
//!     "SELECT id, title, read_count FROM volumes \
//!      WHERE tenant_id = $1 AND (\"read_count\" > $3 AND \"title\" LIKE $4 ESCAPE \'!\') \
//!      ORDER BY \"title\" DESC, \"id\" ASC LIMIT $2",
//! );
//!
//! // let rows = query.build_query_as::<Volume>()?.fetch_all(&pool).await?;
//!
//! // The token for the next page is read out of the last row:
//! //   Cursor::new(&sort).after(last, &schema)?
//! // which needs a live row, so see `tests/sqlite.rs` for it end to end.
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

#[cfg(feature = "cel")]
mod cel;
mod cursor;
mod dialect;
mod error;
#[cfg(feature = "cel")]
mod filter;
mod fragment;
mod schema;
mod sort;
mod splice;
mod template;
mod value;

pub use cursor::{Cursor, CursorKey};
pub use dialect::Dialect;
pub use error::Error;
#[cfg(feature = "cel")]
#[cfg_attr(docsrs, doc(cfg(feature = "cel")))]
pub use filter::Filter;
pub use fragment::QueryFragment;
pub use schema::{Column, ColumnType, Schema, Table};
pub use sort::{Direction, Sort, SortKey};
pub use splice::{Slot, Splice};
/// Declare a table's allow-list on the struct that describes it.
///
/// Shares its name with the [`Schema`] trait, as `FromRow` does with its own
/// derive -- but this one generates an inherent `schema()` returning a
/// `&'static `[`Table`], rather than an impl.
#[cfg(feature = "derive")]
#[cfg_attr(docsrs, doc(cfg(feature = "derive")))]
pub use sqlx_query_macros::Schema;
pub use template::QueryTemplate;
pub use value::Value;
