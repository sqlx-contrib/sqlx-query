# sqlx-query

> Adds filtering, ordering and keyset pagination to a SQL query you already
> wrote, for [sqlx](https://github.com/launchbadge/sqlx). Stop rebuilding your
> query in a builder DSL.

[![CI](https://github.com/sqlx-contrib/sqlx-query/actions/workflows/ci.yml/badge.svg)](https://github.com/sqlx-contrib/sqlx-query/actions/workflows/ci.yml)
[![Rust: 1.94+](https://img.shields.io/badge/rust-1.94%2B-dea584.svg)](https://www.rust-lang.org)
[![sqlx: 0.9](https://img.shields.io/badge/sqlx-0.9-4169e1.svg)](https://github.com/launchbadge/sqlx)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE)

A list endpoint takes `filter`, `order_by` and `page_token` from the client, and
none of the three is known when you write the SQL. The usual answers both cost
something: a query builder takes the SQL away from you, and string
concatenation takes the safety. This crate leaves the statement alone and
splices into comments the database already ignores.

So the skeleton stays a statement. It runs in `psql`, it `EXPLAIN`s, and
`skeleton()` hands it to `sqlx::query!` to be checked against a live database at
compile time — none of which a template language with `{}` holes can do.

## Installation

Not on crates.io: the name `sqlx_query` is already taken there by an unrelated
crate, so this is a git dependency until it has one of its own.

```toml
[dependencies]
sqlx-query = { git = "https://github.com/sqlx-contrib/sqlx-query", features = [
  "postgres",
  "cel",
] }
```

| Feature | |
| --- | --- |
| `postgres` | PostgreSQL, `$N` placeholders. On by default. |
| `sqlite` | SQLite, `?` placeholders. |
| `mysql` | MySQL, `?` placeholders. |
| `cel` | [CEL](https://cel.dev) filter expressions — the `Filter` type. |

At least one driver is required: without one there is no `Arguments` to splice
against. Ordering and pagination work without `cel`; only `Filter` needs it.

## Requirements

- Rust 1.94+ — the MSRV sqlx 0.9 declares
- sqlx 0.9+ — `Execute::sql` returns an owned `SqlStr` from 0.9, which is what
  makes a rewritten query expressible at all

## Usage

```rust
use std::sync::LazyLock;

use sqlx::Postgres;
use sqlx_query::{Column, ColumnType, Cursor, Filter, QueryMapping, QueryTemplate, Sort};

// The query you already wrote. One slot takes fragments from several sources --
// here the client's filter and the cursor's seek condition -- joined by its own
// `AND`.
static VOLUMES: LazyLock<QueryTemplate<Postgres>> = LazyLock::new(|| {
    QueryTemplate::parse(
        "SELECT id, title, read_count
           FROM volumes
          WHERE tenant_id = $1
            /* AND query.filter */
          /* ORDER BY query.order */
          LIMIT $2",
    )
    .expect("valid skeleton")
});

// What a request may name, and which column each path resolves to. A template
// is SQL with holes; this is policy, so it stays separate -- the same skeleton
// can serve an administrator and a caller who may see less.
static MAPPING: LazyLock<QueryMapping> = LazyLock::new(|| {
    QueryMapping::new()
        .key("id", ColumnType::Int)        // unique: a token can name one row
        .column("title", ColumnType::Text)
        .add("readCount", Column::new("read_count", ColumnType::Int))
});

// Request parameters, as the strings they arrive as. Each treats an empty
// string as "not asked for" rather than as an error, and each is checked
// against the mapping before it goes anywhere near the query. `resolve` is the
// boundary: on the far side of it nothing is still a client's string.
let filter = Filter::parse(&request.filter)?.resolve(&*MAPPING)?;      // CEL, `cel` feature
let sort = Sort::parse(&request.order_by)?.asc("id").resolve(&*MAPPING)?;  // AIP-132
let cursor = Cursor::parse(&request.page_token)?.resolve(&*MAPPING)?;

// So the builder needs no mapping. `seek` and `order` agree on the ordering or
// the build fails -- a token reused under a changed `order_by` would otherwise
// hand back rows the client has already seen, with no error anywhere.
let rows = VOLUMES
    .builder()
    .bind(tenant_id)   // $1
    .bind(page_size)   // $2
    .filter(&filter)
    .seek(&cursor)
    .order(&sort)
    .build()?
    .fetch_all(&pool)
    .await?;

// Your own `FromRow`, applied by hand, so the raw rows stay available for the
// cursor below.
let page: Vec<Volume> = rows.iter().map(Volume::from_row).collect::<Result<_, _>>()?;

// The token for the next page, read out of the last row by the columns the
// resolved ordering already carries — so there is no second field-to-column
// mapping to keep in step, and the ordering may be one the client chose at
// runtime.
if let Some(last) = rows.last() {
    response.next_page_token = Cursor::new(&sort).after(last)?.as_str().to_owned();
}
```

The first page, with an empty token:

```sql
SELECT id, title, read_count
       FROM volumes
      WHERE tenant_id = $1
        AND ("read_count" > $3 AND "title" LIKE $4 ESCAPE '!')
      ORDER BY "title" DESC, "id" ASC
      LIMIT $2
```

The second, with the token above. Note `LIMIT $2` still means the second bound
value, even though `$3`–`$7` are spliced ahead of it:

```sql
SELECT id, title, read_count
       FROM volumes
      WHERE tenant_id = $1
        AND ("read_count" > $3 AND "title" LIKE $4 ESCAPE '!')
        AND (("title" < $5) OR ("title" = $6 AND "id" > $7))
      ORDER BY "title" DESC, "id" ASC
      LIMIT $2
```

An unfilled or empty slot emits nothing — comment and joiner both — so the first
page's absent seek condition, and an absent filter, simply leave the query as
written.

## How it works

**Placeholders are never rewritten.** A `QueryFragment` stores the SQL *between*
its binds and leaves the placeholder to the driver, written at splice time by
`Arguments::format_placeholder`. So `$3` is produced once, when the value is
added. There is no pass that turns `?` into `$3` afterwards, and so no way for
one to wander into a string literal.

**Numbering is not the same everywhere.** PostgreSQL's `$N` names the *N*th
bound value, so text spliced ahead of a `$2` leaves it alone — that is why
`LIMIT $2` survives above. MySQL's and SQLite's `?` names the *N*th placeholder
*in the text*, so splicing ahead of one shifts it. Where that bites — binding
after filling, filling slots out of order, or a skeleton whose own `?` sits
after a slot — this crate returns an error rather than a wrong answer.

**The mapping belongs to the query, not the table.** Qualifiers and aliases are
facts about one query's `SELECT` and `FROM` — a join needs `a.name`, and
`SELECT a.name AS author_name` means filtering and reading the row use different
strings. So two queries over one table can expose different surfaces, and the
mapping is declared rather than derived from a row struct.

It is also an allow-list, not a description. One generated from every column
would hand clients the ability to filter and sort on anything, which is what
fail-closed exists to prevent.

**The mapping is the type checker.** cel-rust parses without checking, so
`id > 'tuesday'` is a perfectly good CEL program. The allow-list is the only
thing that can reject it before the database does, which is why the two are the
same object.

## Status

Early, but the core is exercised against all three servers. `tests/sqlite.rs`,
`tests/postgres.rs` and `tests/mysql.rs` each page through a table and check
that every row is visited exactly once, including rows that tie on the sort
column, under ascending, descending and mixed orderings, with and without a
filter, and by a timestamp key that has to survive the token codec and the
driver's own wire format.

The two numbering rules are checked where they differ rather than asserted
against a string: PostgreSQL runs the skeleton with a `LIMIT $2` that four
spliced placeholders are pushed in front of, MySQL runs one where seven values
have to land on seven `?` in text order.

SQLite runs in memory, so it needs nothing. PostgreSQL and MySQL read
`SQLX_QUERY_POSTGRES_URL` and `SQLX_QUERY_MYSQL_URL` and **skip** when unset —
see [Development](#development).

The rationale lives with the code — `cargo doc --open` — rather than here.

## Development

sqlx 0.9 declares `rust-version = "1.94"`, so this crate does too.
`rust-toolchain.toml` pins the dev toolchain to 1.95.0, so plain `cargo` picks
the right one even when the machine's default stable is older than the MSRV.

Open the repository in a Dev Container, or on the host:

```sh
nix develop
make test
make lint
```

Either way you get PostgreSQL and MySQL. The Dev Container's
`docker-compose.yml` runs both; `nix develop` runs
[`devcontainer-env`](https://github.com/devcontainer-env/devcontainer-env) in
its `shellHook`, which reads the same `devcontainer.json` and exports
`SQLX_QUERY_POSTGRES_URL` and `SQLX_QUERY_MYSQL_URL` with the container
hostnames rewritten to whichever ports Docker published — so one compose file
serves the container and the host, and neither pins a port that the next
project would collide with.

With no stack running, both variables are unset and those tests skip rather
than fail. That keeps `cargo test` green on a machine with no Docker, at the
cost of making a skipped suite look exactly like a passing one. `echo
$SQLX_QUERY_POSTGRES_URL` is the check.

One crate. There was briefly a `sql!` macro that scanned the skeleton at compile
time, which forced two more — a proc-macro crate cannot export the scanner it
needs, and the scanner could not stay here because the macro crate would then
depend on the crate depending on it. It was dropped: with the scanner
deliberately treating an unrecognised sentinel as prose, all `sql!` caught at
build time was an unterminated comment or a duplicate slot name, neither of
which survives the first test.

Driver-specific tests are gated on their feature, so a build with one driver
and no default features — which is what consumers take — runs a narrower suite.
`make lint` walks all six of those configurations as well as the all-features
one, because that is where breakage hides: twice now the narrow builds have
failed while the wide one stayed green.

`clippy::all` and `clippy::pedantic` are denied rather than warned, because
several consumers in this ecosystem deny pedantic at the workspace level: a lint
this crate tolerates is one they cannot.

## License

[MIT](LICENSE)
