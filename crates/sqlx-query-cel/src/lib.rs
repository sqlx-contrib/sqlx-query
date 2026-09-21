//! `Filter` (CEL -> `WHERE` fragment) belongs here, implementing
//! [`sqlx_query::QueryResolver`] the same way
//! [`sqlx_query::OrderByClause`] does, and `impl
//! From<Filter> for sqlx_query::WhereClause` so it plugs into
//! `QueryComposer::push_where` — on top of `sqlx-cel`, the crate this is a
//! port of.
//!
//! Not implemented yet: as of this pass, `sqlx-cel` isn't reachable —
//! it's absent from the `sqlx-contrib` org and unpublished on crates.io
//! (`sqlx-aip`, which depends on it, is confirmed local-only). This crate
//! is scaffolded so the workspace builds; `Filter` should be built on top
//! of `sqlx-cel` once it's vendored, pointed at a real path/git ref, or
//! published, rather than reimplemented from a raw CEL crate — keeping
//! the dependency direction one-way (`sqlx-query` must never depend on
//! `sqlx-query-cel`) is what makes that the right call.
