# sqlx-query

> Splices SQL fragments into the sentinel comments of a query you already wrote,
> for [sqlx](https://github.com/launchbadge/sqlx).

[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE)

The skeleton is a statement. Comments are inert, so it runs in `psql`, it
`EXPLAIN`s, and `skeleton()` hands it to `sqlx::query!` to be checked against a
live database at compile time — none of which a template language with `{}`
holes can do.

## The whole API

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

## Three things worth knowing

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

Early, but the core is exercised against a real database: `tests/sqlite.rs`
pages through a table in memory and checks that every row is visited exactly
once, including rows that tie on the sort column, under ascending, descending
and mixed orderings. PostgreSQL and MySQL are still only covered by SQL-shape
assertions — the numbering rules below are asserted against generated SQL, not
against a live server.

The rationale lives with the code — `cargo doc --open` — rather than here.

## Development

sqlx 0.9 declares `rust-version = "1.94"`, so this crate does too.
`rust-toolchain.toml` pins the dev toolchain to 1.95.0, so plain `cargo` picks
the right one even when the machine's default stable is older than the MSRV.

```sh
cargo test --features cel,sqlite,mysql
cargo clippy --all-targets --features cel,sqlite,mysql
```

One crate. There was briefly a `sql!` macro that scanned the skeleton at compile
time, which forced two more — a proc-macro crate cannot export the scanner it
needs, and the scanner could not stay here because the macro crate would then
depend on the crate depending on it. It was dropped: with the scanner
deliberately treating an unrecognised sentinel as prose, all `sql!` caught at
build time was an unterminated comment or a duplicate slot name, neither of
which survives the first test.

Driver-specific tests are gated on their feature. `clippy::all` and
`clippy::pedantic` are denied rather than warned, because several consumers in
this ecosystem deny pedantic at the workspace level: a lint this crate tolerates
is one they cannot.

## License

[MIT](LICENSE)
