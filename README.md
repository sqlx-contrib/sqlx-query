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
  filter and sort on columns you offered by name.
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
- [Features](#features)
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
│  WHERE ((rank) > ($3)) AND tenant_id = $1                 │
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
`||`, `!`, the six comparisons, `in` over a list, arithmetic, and literals —
plus the string methods `startsWith`, `endsWith` and `contains`, and the
constructors `timestamp("...")` and `uuid("...")`.

| Filter                                  | Becomes                                     |
| --------------------------------------- | ------------------------------------------- |
| `rank > 10`                             | `(rank) > ($1)`                             |
| `name == 'alice' \|\| rank >= 50`       | `((name) = ($1)) OR ((rank) >= ($2))`       |
| `status in ['ACTIVE', 'PENDING']`       | `status IN ($1, $2)`                        |
| `deleted_at == null`                    | `deleted_at IS NULL`                        |
| `name.startsWith('Gro')`                | `(name) LIKE $1 ESCAPE '!'`, bound `Gro%`   |
| `name.contains('50%')`                  | `(name) LIKE $1 ESCAPE '!'`, bound `%50!%%` |
| `created > timestamp('2026-01-01T00:00:00Z')` | `(created) > ($1)`, bound as a timestamp |
| `id == uuid('0123…cdef')`               | `(id) = ($1)`, bound as a UUID              |

A blank filter parses to the empty filter, as a blank ordering does to the empty
clause: it resolves against any mapping and renders as an empty `WhereClause`,
which leaves its slot empty — so a request's `filter` goes through the same
parse and resolve whether or not it says anything. An empty `WhereClause` is a
valid value throughout: `and` and `or` return the other side, and a query with
no `where` slot takes one without complaint.

Two deliberate choices: `== null` renders as `IS NULL`, because `= NULL` is
never true and so never what was meant; and both sides of a binary operator are
always parenthesized, so there is no precedence table to get wrong.

The string methods are called on a field with a string literal, and the literal
is bound as the pattern with its own `%`, `_` and `!` escaped, so it matches
itself. The escape is `!` rather than `\`, because a backslash inside a string
literal is itself an escape in MySQL's default mode. Case sensitivity is the
engine's: PostgreSQL's `LIKE` is case-sensitive, SQLite's ignores ASCII case, and
MySQL's follows the column's collation.

`timestamp("...")` reads an RFC 3339 string, offset and all, into a timestamp
bind value, so it compares with a timestamp column where a plain string
wouldn't: PostgreSQL has no `timestamptz > text`. It is read when the filter is
parsed, so its argument has to be a literal, and a malformed one is an
`InvalidLiteral` error rather than a query the database refuses. It needs one
of `sqlx-query-cel`'s date-library features, `chrono` or `time`, which turn on
`sqlx-query`'s of the same name; without either it is refused. `uuid("...")` is
the same for a `uuid` column — PostgreSQL has no `uuid = text` either — behind
the `uuid` feature.

Macros, comprehensions, other function calls, maps and structs are refused. They
have no reading as a `WHERE` clause, and guessing one would be inventing SQL the
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

A `Pager` runs the page loop around it. It knows the page size and the
ordering, asks the query for one row past the page, and cuts what comes back
into a `Page` — the rows, and the cursor to the next page if that extra row
came back:

```rust
let pager = Pager::new(Cursor::new(order_by.clone()), 50);

let mut query = QueryComposer::<Postgres>::new(LIST_USERS);
query
    .bind_value(tenant_id)
    .bind_value(pager.limit()) // 51: one past the page
    .push_order_by(order_by);
if let Some(cursor) = cursor {
    query.with_cursor(cursor); // where the previous page left off
}

let rows = query.build()?.fetch_all(&pool).await?;
let page = pager.next_page(rows)?; // Page { rows: at most 50, cursor }

// The cursor's values come off the page's last row, so no one has to know
// each key's Rust type. `None` on the last page.
let next_page_token = page.cursor.map(|cursor| cursor.encode());

// ... and on the next request:
let cursor = Some(Cursor::parse(&page_token)?);
```

A page that exactly fills the size is the last one: the extra row is what says
there is another. `Cursor::after_row` builds the same cursor by hand, from
whichever row a caller picks.

A key can be any column a `Value` holds, a UUID primary key included behind
the `uuid` feature — it is read back off the row as a `Value::Uuid` and bound
against the column on the next page.

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

`bind_value` takes one of `Value`'s eight kinds — null, bool, int, float,
string, timestamp, bytes, UUID — and stays visible in `compose()`'s output, so a
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

## Features

Nothing is on by default. Name the driver you use and the date library you
already have, and the others are never compiled:

```toml
sqlx-query = { version = "0.1", features = ["postgres", "chrono"] }
```

| Feature                        | What it turns on                                         |
| ------------------------------ | -------------------------------------------------------- |
| `postgres`, `mysql`, `sqlite`  | `QueryDialect` for that driver — at least one is needed  |
| `chrono`                       | `From<DateTime<Utc>>`, and timestamps read out of a row  |
| `time`                         | the same for `time::OffsetDateTime`                      |
| `uuid`                         | `From<uuid::Uuid>`, UUIDs bound as `uuid` and read out of a row |

`Value::Timestamp` holds microseconds since the epoch rather than a date
type, and exists in every build regardless of features. A page token is a
serialized `Value`, and postcard writes an enum variant by *index* — so a
variant that compiled out in one build would shift the ones after it and
decode as the wrong thing in another. The token format can't depend on which
date library a consumer picked, because the service that mints a token needn't
be the one that redeems it.

With both `chrono` and `time` on, `chrono` is what a timestamp is bound and
decoded as. Arbitrary, but it has to be one of them, and such a build reads
either.

`Value::Uuid` is the same arrangement: sixteen bytes in every build, bound and
decoded as a `uuid::Uuid` behind the `uuid` feature. It is also why a new
variant only ever goes last — every other one keeps its index, so a token
minted before it still reads.

`sqlx-query-cel` has features of its own for the filter constructors: `chrono`
or `time` for `timestamp("...")` and `uuid` for `uuid("...")`, each turning on
`sqlx-query`'s of the same name.

## Limitations

**`resolve()` is optional, and skipping it widens what a client may name.**
`parse` and `resolve` return the same type, so an unresolved clause still
splices. `parse` guarantees the field is an identifier — `(select(1))` and
`rank;drop` are refused — so no expression reaches the SQL either way. What
you lose by skipping `resolve` is the *restriction*: any column that exists
becomes sortable, including ones you never meant to offer.

```rust
// Fine when the client's names are your column names and every column is
// fair game.
query.push_order_by(OrderByClause::parse(&request.order_by)?);

// Fail-closed: only the fields you listed, renamed to the columns you chose.
query.push_order_by(OrderByClause::parse(&request.order_by)?.resolve(&columns)?);
```

`FilterClause` is the same, minus the identifier question — CEL's own lexer
only yields identifier-shaped tokens.

**A cursor pages on a `uuid` key only with the `uuid` feature.** Without it
a PostgreSQL `uuid` is none of the kinds `Cursor::after_row` can read, so it
fails with `RowValueUndecodable`.

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
- [`chrono`](https://crates.io/crates/chrono) or
  [`time`](https://crates.io/crates/time) for timestamp values — optional,
  and only for converting to and from `Value::Timestamp`'s integer
- [`thiserror`](https://crates.io/crates/thiserror) for the error types

Tooling: Nix for the dev shell, and a Dev Container that reuses the same
flake and stands up both servers.

## License

[MIT](LICENSE)

<!-- markdownlint-disable-file MD013 -->
