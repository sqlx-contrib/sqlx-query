# sqlx-query

> Add filtering, ordering and keyset pagination to a SQL query you already wrote. No query builder, no DSL — your SQL keeps its shape, and the pieces get spliced into comments you left for them.

[![CI](https://github.com/sqlx-contrib/sqlx-query/actions/workflows/ci.yml/badge.svg)](https://github.com/sqlx-contrib/sqlx-query/actions/workflows/ci.yml)
[![Rust (edition 2021)](https://img.shields.io/badge/Rust-2021-black?logo=rust)](https://www.rust-lang.org/)
[![Nix Flake](https://img.shields.io/badge/Nix-Flake-5277C3?logo=nixos&logoColor=white)](https://nixos.wiki/wiki/Flakes)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE)

> [!NOTE]
> **Pre-1.0.** Neither crate is on crates.io yet and the API may still move.
> What is here works and is tested against real PostgreSQL, MySQL and SQLite
> servers — see [Limitations](#limitations) for what it doesn't do.

## Why

The usual answer to "the client can filter and sort" is a query builder: you
stop writing SQL and start writing Rust that emits SQL. That trade costs you
the thing SQL is good at. A builder's output is hard to read, impossible to
paste into `psql`, and drifts from what you meant one `.and_where()` at a time.

So the query stays yours, as a string, with comments marking the two places a
request is allowed to reach:

```sql
SELECT id, name, rank, created_at
  FROM users
 WHERE /* query.where AND */ tenant_id = $1
 ORDER BY /* query.order_by , */ id
 LIMIT $2
```

That query runs as-is — the slots are comments, so an empty filter is not a
special case, it is just a comment nobody replaced. What you get over a
builder:

- The base query is still a static string. It can be checked, explained, and
  pasted into a client, because nothing assembled it.
- A filter is parsed, not concatenated. Every literal becomes a bind value; no
  request text ever reaches the SQL.
- Fields are resolved against a fail-closed allow-list, so a request can only
  filter and sort on columns you offered by name — provided you call
  `resolve()`, which nothing yet forces (see [Limitations](#limitations)).
- The placement of the clause is your decision, not the library's. A slot
  inside a CTE, a sub-select, or one of two `UNION` arms goes exactly where you
  put it.

What it is *not*: a way to build a query you haven't written. There is no
`SELECT` generation, no table introspection, and no joins inferred from
anything. If you don't already have the query, this crate has nothing to add to
it.

## Table of contents

- [Why](#why)
- [The crates](#the-crates)
- [How it works](#how-it-works)
- [Quick start](#quick-start)
- [Filtering](#filtering)
- [Ordering](#ordering)
- [Keyset pagination](#keyset-pagination)
- [Binding](#binding)
- [Dialects](#dialects)
- [Limitations](#limitations)
- [Development](#development)
- [Dependencies](#dependencies)
- [License](#license)

## The crates

| Crate                                     | What it is                                                                          |
| ----------------------------------------- | ----------------------------------------------------------------------------------- |
| [`sqlx-query`](crates/sqlx-query)         | The composer, the clause types, and the cursor. No filter language, no CEL.         |
| [`sqlx-query-cel`](crates/sqlx-query-cel) | Reads an AIP-160 `filter` written in [CEL] into a `WhereClause` the composer takes. |

The split is the point: `sqlx-query` never depends on the filter language.
Anything `WHERE`-shaped converts *into* a `WhereClause`, so a different filter
syntax is a new crate rather than a fork.

[CEL]: https://github.com/google/cel-spec

## How it works

```
  request: filter = "rank > 10", order_by = "rank desc", page_token = "…"
                   │
                   ▼
┌───────────────────────────────────────────────────────────┐
│ SELECT id, name FROM users                                │  your base query,
│  WHERE /* query.where AND */ tenant_id = $1               │  a static string
│  ORDER BY /* query.order_by , */ id                       │
└───────────────────────────────────────────────────────────┘
                   │  resolve → render → splice
                   ▼
┌───────────────────────────────────────────────────────────┐
│ SELECT id, name FROM users                                │  what executes
│  WHERE (rank) > ($3) AND tenant_id = $1                   │
│  ORDER BY rank DESC , id                                  │
└───────────────────────────────────────────────────────────┘
  binds: [tenant_id, 50, 10]
```

Three things happen on the way through:

1. **Resolve.** Each field a request names is looked up in a `&HashMap<&str,
   &str>` of allowed field → column. A miss is an error, not a pass-through.
2. **Render.** The condition becomes SQL text plus a flat list of bind values,
   numbered locally (`$1`, `$2`, …) as if the fragment were the whole query.
3. **Splice.** Each slot is replaced by its fragment, and the fragment's
   placeholders are shifted past the base query's own — so a fragment written
   with local numbering lands correctly no matter how many values the base
   query already had. A slot with nothing to put in it is dropped whole,
   trailing connective and all; a clause with no slot to go into is an
   error, not a silent drop. On a `?` dialect the whole statement is then
   renumbered back to bare `?` (see [Dialects](#dialects)).

## Quick start

```rust
use std::collections::HashMap;

use sqlx::Postgres;
use sqlx_query::{OrderByClause, QueryComposer, QueryResolver};
use sqlx_query_cel::FilterClause;

// Left where it can be read: the slots are comments, so this runs as-is.
const LIST_USERS: &str = "
    SELECT id, name, rank, created_at
      FROM users
     WHERE /* query.where AND */ tenant_id = $1
     ORDER BY /* query.order_by , */ id
     LIMIT $2
";

// The allow-list. Field names as the client says them, columns as the table
// spells them -- anything absent is refused rather than passed through.
let columns = HashMap::from([
    ("name", "name"),
    ("rank", "rank"),
    ("created", "created_at"),
]);

let filter = FilterClause::parse("rank > 10 && name != 'root'")?.resolve(&columns)?;
let order_by = OrderByClause::parse("rank desc, created asc")?.resolve(&columns)?;

let mut query = QueryComposer::<Postgres>::new(LIST_USERS);
query
    .bind_value(tenant_id)      // the base query's own $1
    .bind_value(50i64)          // ... and $2
    .push_where(filter)   // -> /* query.where AND */
    .push_order_by(order_by); // -> /* query.order_by , */

let users = query.build()?.fetch_all(&pool).await?;
```

`compose()` instead of `build()` hands back the SQL text and bind values
without touching a connection, which is what the tests in this repo use:

```rust
let statement = query.compose()?;
let (sql, arguments) = (statement.sql(), statement.arguments());
```

## Filtering

`FilterClause::parse` accepts the comparison-and-boolean part of [CEL]: `&&`,
`||`, `!`, the six comparisons, `in` over a list, arithmetic, and literals.

| Filter                                  | Becomes                                     |
| --------------------------------------- | ------------------------------------------- |
| `rank > 10`                             | `(rank) > ($1)`                             |
| `name == 'alice' \|\| rank >= 50`       | `((name) = ($1)) OR ((rank) >= ($2))`       |
| `status in ['ACTIVE', 'PENDING']`       | `status IN ($1, $2)`                        |
| `deleted_at == null`                    | `deleted_at IS NULL`                        |

Two deliberate choices: `== null` renders as `IS NULL`, because `= NULL` is
never true and so never what was meant; and both sides of a binary operator are
always parenthesized, so there is no precedence table to get wrong.

Macros, comprehensions, function calls, maps and structs are refused. They have
no reading as a `WHERE` clause, and guessing one would be inventing SQL the
caller didn't ask for.

## Ordering

`OrderByClause::parse` reads AIP-132's `"field [asc|desc], ..."` — a plain
field list, no CEL:

```rust
OrderByClause::parse("rank desc, created asc")?.resolve(&columns)?;
// -> rank DESC, created_at ASC
```

An empty string parses to an empty clause, which drops its slot rather than
erroring. Clauses accumulate as tie-breakers: `push_order_by` twice means "sort
by the first, **then** by the second".

## Keyset pagination

A `Cursor` is a resolved `OrderByClause` plus one boundary value per key,
rendered as the OR-of-ANDs tuple comparison the seek method wants — not
`OFFSET`, which re-reads every row it skips.

```rust
// The page just fetched, turned into the cursor for the next one. Values come
// off the last row, so no one has to know each key's Rust type.
let cursor = Cursor::new(order_by.clone()).after_row(users.last().unwrap())?;
let page_token = cursor.encode();

// ... and on the next request:
let cursor = Cursor::parse(&page_token)?;
let mut query = QueryComposer::<Postgres>::new(LIST_USERS);
query.bind_value(tenant_id).bind_value(50i64).with_cursor(cursor);
```

The token is the cursor `postcard`-serialized, checksummed and base64'd,
following `einride/aip-go`'s `pagination.PageToken` shape — opaque, and cleanly
rejected if hand-edited. A cursor carries the `order_by` it was built against,
so it doesn't have to be repeated; if it *is* set and disagrees, `compose()`
fails rather than paging through a different sort than the token was cut for.

## Binding

Two ways to supply a value for one of the base query's own placeholders.

```rust
query.bind_value(50i64);              // a scalar the composer can show back
query.bind(uuid::Uuid::new_v4());     // anything sqlx can encode
```

`bind_value` takes one of `Value`'s seven kinds — null, bool, int, float,
string, timestamp, bytes — and stays visible in `compose()`'s output, so a
test or a log line can read it back.

`bind` takes anything satisfying `sqlx::Encode + Type`: a `Uuid`, a
`serde_json::Value`, a `BigDecimal`, your own `#[derive(sqlx::Type)]` newtype.
The composer never sees the value, so `QueryArgument::value()` answers `None`
for it and `compose()` can only report that an argument is there.

Clause literals are always the visible kind. They come from a parsed filter,
so they're scalars by construction — and a [`Cursor`](#keyset-pagination)
serializes them into its page token, which a boxed encoder could not be.

## Dialects

`QueryComposer<DB>` is generic over a `QueryDialect`, implemented for
`Postgres`, `MySql` and `Sqlite`. The distinction that matters is how a
placeholder names its value: Postgres's `$N` carries a number, while MySQL's
and SQLite's bare `?` is positional by where it sits in the text.

A `?` can't be written down before its position in the finished statement is
known — and a fragment's position isn't known until it has been spliced, which
is after it was built. So everything is numbered on the way through: for a `?`
dialect the base query's `?` placeholders are numbered first (`?` → `$1`, `$2`, … in
textual order), the clauses are spliced and shifted as if it were Postgres, and
the numbering is converted back to `?` in one final pass. That last pass emits
one value per *reference*, so a number used twice — a cursor reuses each
boundary value — becomes two `?`s and two copies of the value.

The consequence worth knowing: on a `?` dialect the bind list comes out in
textual order, so a slot ahead of the base query's own `?` placeholders binds ahead
of them too. On Postgres nothing moves and the list stays in
bind-declaration order.

Each dialect is also lexed by its own rules — backtick and bracket identifiers,
`#` comments, backslash escapes, nested block comments — so a `?` inside a
string literal or a quoted identifier is text, not a placeholder.

## Limitations

**`resolve()` is enforced by convention, not by the type.** `parse` and
`resolve` return the same type, so a clause that was never resolved can still
be spliced — and `OrderByClause::parse` does not validate identifiers, so a
whitespace-free expression reaches the SQL:

```rust
// Do not do this: `resolve` is what applies the allow-list.
query.push_order_by(OrderByClause::parse(&request.order_by)?);
//        ORDER BY (select(1)) ASC , id
```

Always `parse(...)?.resolve(&columns)?` for anything a client supplied. A
resolved/unresolved distinction in the type system is the fix, and is the
next thing planned.

**A cursor cannot page on a `uuid` key.** Cursor keys are read back off the
row into a `Value`, whose kinds are bool, int, float, string, timestamp and
bytes; a PostgreSQL `uuid` is none of them, so `Cursor::after_row` fails with
`RowValueUndecodable`. Keyset pagination therefore needs an integer, text or
timestamp sort key today. Parameters are unaffected — `bind` takes a `Uuid`
fine; it's only sorting on one that doesn't work.

**No `SELECT` generation.** By design, and worth repeating: this splices into
a query you wrote. It does not write one.

## Development

The flake's dev shell has the pinned toolchain (see `rust-toolchain.toml`), so
`nix develop` and the Dev Container are the same compiler:

```bash
nix develop                  # or open the folder in a Dev Container
make test                    # cargo test --workspace, then the doctests
make lint                    # cargo fmt --check + clippy (all + pedantic, denied)
make doc-check               # cargo doc with warnings as errors, no browser
make doc                     # the same, opened in a browser
```

`make test` needs no server. Most of the suite compares rendered SQL, and the
SQLite half of the live-driver tests runs in memory — which matters, because
comparing strings cannot tell you whether a `?` bound the value you meant. The
PostgreSQL and MySQL halves skip themselves unless their URLs are set:

```bash
make test-servers       # starts both in Docker, runs everything
make test-servers-down  # and stops them
```

The Dev Container sets both URLs, so `make test` covers everything inside it.

## Dependencies

- [`sqlx`](https://crates.io/crates/sqlx) for `SqlStr`, `Arguments` and the
  driver marker types
- [`cel`](https://crates.io/crates/cel) for the filter syntax —
  `default-features = false`, because this parses CEL and never evaluates it
- [`regex`](https://crates.io/crates/regex) to find the slots
- [`postcard`](https://crates.io/crates/postcard) +
  [`base64`](https://crates.io/crates/base64) for the page token
- [`chrono`](https://crates.io/crates/chrono) for timestamp bind values
- [`thiserror`](https://crates.io/crates/thiserror) for the error types

Tooling: Nix for the dev shell, and a Dev Container that reuses the same
flake and stands up both servers.

## License

[MIT](LICENSE)

<!-- markdownlint-disable-file MD013 -->
