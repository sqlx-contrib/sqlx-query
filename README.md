# sqlx-query

> Splice a SQL fragment into the sentinel comment of a query you already wrote —
> the connective stays with the statement, and unspliced it still runs.

[![CI](https://github.com/sqlx-contrib/sqlx-query/actions/workflows/ci.yml/badge.svg)](https://github.com/sqlx-contrib/sqlx-query/actions/workflows/ci.yml)
[![Crate](https://img.shields.io/crates/v/sqlx-query)](https://crates.io/crates/sqlx-query)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE)

Substitutes SQL fragments into the comments of a statement, for
[sqlx](https://github.com/launchbadge/sqlx).

The Rust counterpart of [pgxquery](https://github.com/pgx-contrib/pgxquery), and
the third of the three: [sqlx-cel](https://github.com/sqlx-contrib/sqlx-cel)
makes a fragment out of a CEL expression,
[sqlx-aip](https://github.com/sqlx-contrib/sqlx-aip) makes two out of an AIP
`List` request, and this puts one in a query. It depends on neither — a
`format!` produces a fragment just as well.

```rust
use sqlx_query::splice;

const LIST_VOLUMES: &str = "\
SELECT * FROM volumes
WHERE /* query.where AND */ TRUE
ORDER BY /* query.order_by , */ id
LIMIT $1 OFFSET $2";

let sql = splice(LIST_VOLUMES, &[
    ("where", Some(r#""title" = $3"#)),
    ("order_by", Some(r#""created_at" DESC"#)),
])?;

// SELECT * FROM volumes
// WHERE "title" = $3 AND TRUE
// ORDER BY "created_at" DESC , id
// LIMIT $1 OFFSET $2
```

## The connective belongs to the statement

Whatever else is inside the sentinel is kept, on the side it was written:
`/* query.where AND */` substitutes to `<fragment> AND`, and
`/* query.order_by , */` to `<fragment> ,`.

That is the whole trick. The author of the statement decides how a fragment
joins to what surrounds it, so a fragment never has to know — the same one
splices into a `WHERE` that is `AND`ed and one that is `OR`ed, and into an
`ORDER BY` ahead of a primary-key tiebreaker.

```rust
splice("WHERE archived /* OR query.where */", &[("where", Some("a = $1"))])?;
// WHERE archived OR a = $1
```

## Unspliced, it is a comment

A sentinel whose fragment is `None` is removed, connective and all, which leaves
`WHERE TRUE` on an unfiltered list. And a statement nobody splices at all runs
exactly as written, sentinels included — they are comments.

That is what makes the convention safe to put in checked-in or generated SQL.
The file stays valid for `psql`, for `sqlc`, and for whatever else reads it.

## What is an error, and what is not

A sentinel this call says nothing about is **left alone**. Substituting what you
were not given would be deciding the statement is wrong.

A fragment with **no sentinel to go into is an error**. The predicate would
silently not apply, and a dropped predicate widens a result set rather than
emptying it — the kind of bug that returns plausible rows and reaches
production. `Error::MissingSentinel` names the fragment instead.

## Placeholders

A spliced fragment lands among the statement's own parameters, and the two have
to agree. Two ways, and the good one costs nothing:

**Ask the producer to start where the statement stops.** sqlx-cel's `Options`
and sqlx-aip's `rewrite_with` both take a `param_offset`, and
`placeholder_count` is what you pass them:

```rust
let offset = sqlx_query::placeholder_count(LIST_VOLUMES) + 1; // 3
```

**Or renumber afterwards** with `shift`, for a fragment that arrived numbered
from `$1` and cannot be asked to start elsewhere:

```rust
assert_eq!(sqlx_query::shift(r#""title" = $1"#, 2), r#""title" = $3"#);
```

Both read the SQL properly rather than reaching for a regex. A `$1` inside a
string literal, a quoted identifier, a comment or a dollar-quoted body is text,
not a parameter, and renumbering it produces SQL that still parses and binds the
wrong value:

```rust
// The literal is left alone; only the parameter moves.
assert_eq!(
    sqlx_query::shift("note = 'costs $9' AND id = $1", 4),
    "note = 'costs $9' AND id = $5",
);
```

## Positional dialects

All of the above assumes numbered placeholders. With SQLite's or MySQL's `?`,
binds match the *text* rather than a number, so a fragment spliced into the
middle of a statement needs its values bound in the middle of the list too.
`shift` has nothing to do there and `placeholder_count` returns zero.

Splicing still works; the bookkeeping moves to you. Splice at the end, use a
numbered dialect, or count the placeholders either side of the sentinel
yourself.

## Is this safe?

It concatenates strings into SQL, so: exactly as safe as what you hand it. A
fragment from sqlx-cel or sqlx-aip carries literals as placeholders and column
names from a fail-closed allow-list, and is safe to splice. A fragment built by
interpolating a request field is an injection, and nothing here changes that.
sqlx says the same by making you write `AssertSqlSafe` around the result, which
is a sentence you are asserting rather than a cast.

## Scope

**In.** Substituting named fragments into sentinel comments. Counting a
statement's placeholders. Renumbering a fragment's.

**Out.** Building SQL, knowing what a `WHERE` clause is, binding values
(sqlx-cel's `BindAll` does that), talking to a database, and parsing SQL beyond
knowing where text ends.

The crate has no dependencies — not even sqlx. It takes a `&str` and returns a
`String`.

## Development

sqlx 0.9 declares `rust-version = "1.94"`, so this crate does too.
`rust-toolchain.toml` pins the dev toolchain to 1.95.0, so plain `cargo` picks
the right one even when the machine's default stable is older than the MSRV.

```sh
cargo test
cargo clippy --all-targets
```

`tests/postgres.rs` needs a database and skips without `DATABASE_URL`. It is
where the claim that a spliced statement *runs* is checked — text assertions
cannot tell a query bound one slot out from a correct one, because both return
rows.

There is a Nix flake and a devcontainer for a batteries-included shell — the
pinned toolchain, `psql`, and a Postgres to run against:

```sh
devcontainer up --workspace-folder .   # brings up Postgres
nix develop
```
