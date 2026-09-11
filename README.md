# sqlx-query

> Splices SQL fragments into the sentinel comments of a query you already wrote,
> for [sqlx](https://github.com/launchbadge/sqlx).

[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE)

```rust
use sqlx::Postgres;
use sqlx_query::{QueryFragment, QueryTemplate};

let template = QueryTemplate::<Postgres>::parse(
    "SELECT id, title FROM volumes
      WHERE tenant_id = $1
        /* AND query.predicate */
      /* ORDER BY query.order */
      LIMIT $2",
)?;

let query = template
    .splice()
    .bind(tenant_id)
    .bind(page_size)
    .fill("predicate", &recent)
    .fill("order", &by_title)
    .build_query_as::<Volume>()?;
```

The skeleton is a statement. Comments are inert, so it runs in `psql`, it
`EXPLAIN`s, and `skeleton()` hands it to `sqlx::query!` to be checked against a
live database at compile time — none of which a template language with `{}`
holes can do.

## Status

Early. The splicing core is here: templates, the scanner, fragments and
splices. The schema, dialects, filters, sorting and keyset pagination are not
yet. The rationale lives with the code — `cargo doc --open` — rather than here.

## Two things worth knowing

**Placeholders are never rewritten.** A `QueryFragment` stores the SQL *between*
its binds and leaves the placeholder to the driver, written at splice time by
`Arguments::format_placeholder`. So `$3` is produced once, when the value is
added. There is no pass that turns `?` into `$3` afterwards, and so no way for
one to wander into a string literal.

**Numbering is not the same everywhere.** PostgreSQL's `$N` names the *N*th
bound value, so text spliced ahead of a `$2` leaves it alone. MySQL's and
SQLite's `?` names the *N*th placeholder *in the text*, so splicing ahead of one
shifts it. Where that bites — binding after filling, or filling slots out of
order — this crate returns an error rather than a wrong answer.

## Development

sqlx 0.9 declares `rust-version = "1.94"`, so this crate does too.
`rust-toolchain.toml` pins the dev toolchain to 1.95.0, so plain `cargo` picks
the right one even when the machine's default stable is older than the MSRV.

```sh
cargo test
cargo clippy --all-targets
```

`clippy::all` and `clippy::pedantic` are denied rather than warned, because
several consumers in this ecosystem deny pedantic at the workspace level: a lint
this crate tolerates is one they cannot.

## License

[MIT](LICENSE)
