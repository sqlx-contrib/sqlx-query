//! `Filter` (CEL -> `WHERE` fragment) belongs here, implementing
//! [`sqlx_query_v2::QueryResolver`] and [`sqlx_query_v2::QueryFragment`]
//! the same way [`sqlx_query_v2::OrderBy`] does, on top of `sqlx-cel`
//! (see DESIGN.md's "Prior art this is a port of").
//!
//! Not implemented yet: as of this pass, `sqlx-cel` isn't reachable —
//! it's absent from the `sqlx-contrib` org and unpublished on crates.io
//! (`sqlx-aip`, which depends on it, is confirmed local-only per
//! DESIGN.md). This crate is scaffolded so the workspace builds; `Filter`
//! should be built on top of `sqlx-cel` once it's vendored, pointed at a
//! real path/git ref, or published, rather than reimplemented from a raw
//! CEL crate — see DESIGN.md's "Dependency direction" and "Prior art"
//! sections for why.
