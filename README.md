# sqlx-query

Adds filters and ordering to a SQL query you already wrote, by rewriting its
syntax tree, for [sqlx].

```rust
use sqlx::Postgres;
use sqlx_query::QueryWriter;

// The query you already wrote. Nothing in it belongs to this crate -- it runs
// in psql, it EXPLAINs, and `sqlx::query!` will check it against a live
// database.
let mut writer = QueryWriter::<Postgres>::new(
    "SELECT id, title, read_count FROM volumes WHERE tenant_id = $1 ORDER BY id",
)?;

writer
    .bind(7_i64)
    .filter("read_count > 100")
    .sort("title desc")
    .limit(50);

assert_eq!(
    writer.sql()?,
    "SELECT id, title, read_count FROM volumes \
     WHERE tenant_id = $1 AND read_count > 100 \
     ORDER BY title DESC, id LIMIT 50",
);

let rows = writer.build_as::<Volume>()?.fetch_all(&pool).await?;
```

## Two kinds of argument

`filter` and `sort` each take either a SQL fragment, which you wrote and vouch
for, or something a client asked for that was checked against a map of the
fields you chose to offer. The call site shows which.

```rust
use sqlx::Postgres;
use sqlx_query::{QueryWriter, Sort};

// What a request may order by, and the column each name means. Anything not
// here is refused rather than passed through.
let columns = HashMap::from([("title", "title"), ("readCount", "read_count"), ("id", "id")]);

let sort = Sort::parse(&request.order_by)?   // AIP-132: "readCount desc"
    .asc("id")                               // a tiebreaker, so the order is total
    .resolve(&columns)?;                     // fields become columns, or are refused

let mut writer = QueryWriter::<Postgres>::new(
    "SELECT id, title, read_count FROM volumes WHERE tenant_id = $1",
)?;

writer
    .bind(tenant)
    .filter("visible")   // a fragment: yours
    .sort(&sort)         // a request: checked
    .limit(50);

let rows = writer.build_as::<Volume>()?.fetch_all(&pool).await?;
```

```sql
SELECT id, title, read_count FROM volumes WHERE tenant_id = $1 AND visible
ORDER BY "read_count" DESC, "id" ASC
LIMIT 50
```

| argument | checked against the map |
| --- | --- |
| `filter("visible")`, `sort("name asc")` | no -- you wrote it |
| `sort(&sort)` | yes |

A `Sort` that was never resolved is refused rather than written into the query,
so forgetting the step cannot quietly skip the allowlist. A fragment may also
order by an expression -- `sort("lower(name) asc")` -- which a `Sort` cannot,
since it only names columns.

## Filters

A request's `filter` is [CEL], as [AIP-160] describes it, resolved against the
same column map a `Sort` uses.

```rust
let filter = Filter::parse(&request.filter)?   // readCount > 100 && title.startsWith("D")
    .resolve(&columns)?;

writer.bind(tenant).filter(&filter).sort(&sort).limit(50);
```

```sql
SELECT id, title, read_count FROM volumes WHERE tenant_id = $1
  AND "read_count" > $2 AND "title" LIKE $3 ESCAPE '!'
ORDER BY "read_count" DESC, "id" ASC
LIMIT 50
```

Values are bound, never written into the SQL. That is not only about escaping
-- sqlparser escapes what it prints -- but about the query text staying the
same whatever the client searched for, so a prepared statement cache has
something to reuse.

| CEL | SQL |
| --- | --- |
| `==` `!=` `<` `<=` `>` `>=` | the same, either argument order |
| `&&` `\|\|` `!` | `AND` `OR` `NOT`, parenthesised only where precedence needs it |
| `field == null` | `IS NULL` -- nothing equals null in SQL, including null |
| `field in [a, b]` | `IN ($1, $2)` |
| `startsWith` `endsWith` `contains` | `LIKE` with the pattern escaped and bound |
| a bare field | the column, for one that is already boolean |
| `timestamp("…")` | an RFC 3339 date, bound -- needs the `chrono` feature |

Refused rather than guessed at: arithmetic, macros, comparing two literals,
`matches()` (regex is dialect-specific), and `in []` (which matches nothing).

A date is parsed where the request is handled, so `timestamp("soon")` is
refused there rather than surfacing later as a database complaint about a
column nobody mentioned. Without the `chrono` feature, `timestamp()` is simply
a call the language does not have.

SQLite has no timestamp type -- sqlx stores a date as ISO-8601 text -- so the
comparison is over text there. That is right for a column sqlx itself wrote,
and wrong for one holding epoch integers, which nothing here can tell apart.

[CEL]: https://github.com/google/cel-spec
[AIP-160]: https://google.aip.dev/160

## Why a tree and not a template

The query above has no holes, no markers and no escaping. That is the point: a
skeleton with `{}` in it is not SQL, so nothing that reads SQL can read it --
not your formatter, not `EXPLAIN`, not the compile-time check in
`sqlx::query!`. Here the skeleton is the statement, and the parts that vary are
grafted onto its tree.

It also means the rewrite knows what it is editing:

```sql
-- the base query
SELECT id FROM users WHERE a = 1 OR b = 2
```

Appending ` AND role = 'admin'` to that gives `a = 1 OR (b = 2 AND role =
'admin')`, because `AND` binds tighter than `OR`. It is still valid SQL, it
still runs, and it returns rows it should not. A rewriter holding text cannot
see that; one holding a tree cannot miss it, and this one emits:

```sql
SELECT id FROM users WHERE (a = 1 OR b = 2) AND role = 'admin'
```

## What a fragment is allowed to be

Exactly one expression. `filter` parses its argument and then insists the
parser reached the end of it:

| fragment | result |
| --- | --- |
| `role = 'admin'` | accepted |
| `role = 'admin'; DROP TABLE users` | `Error::Trailing` -- a statement is left over |
| `FROM WHERE` | `Error::Trailing` -- parses as the identifier `FROM`, with `WHERE` left over |
| `= = =` | `Error::Fragment` -- not an expression at all |

That is a check on *shape*, not on meaning. `role = 'admin' OR 1=1` is a
perfectly well-formed expression and will be accepted. Build fragments from an
allowlist of columns, with values bound rather than written in.

## Placeholders are numbered, then written back out

Whatever the driver spells them as, placeholders are numbered on the way in and
written back in that driver's form at the end. In between there is one kind of
placeholder and the rewrite is arithmetic: a fragment's `$1` becomes `$3`
because two values were claimed before it.

Values are bound in the order the placeholders claim them -- the base query's
first, then each fragment's -- and sent in that same order, because every
placeholder names the value it wants rather than merely occupying a position:

```
base      SELECT id FROM users WHERE tenant_id = ? LIMIT ?
fragment  role = ?
sqlite    SELECT id FROM users WHERE tenant_id = ?1 AND role = ?3 LIMIT ?2
postgres  SELECT id FROM users WHERE tenant_id = $1 AND role = $3 LIMIT $2
```

A base query uses its own driver's syntax, because it is SQL for that database
and nothing else. PostgreSQL will not parse `?`; SQLite takes `?`, `?N` or `$N`.

## Why not MySQL

Its placeholder is a bare `?`, which takes a value per appearance and has no way
to ask for an earlier one. A fragment spliced into the middle of a query would
shift what every placeholder after it binds, so the values would have to be
reordered to compensate -- and the SQL would look identical either way, so
nothing would show that it had happened.

That is supportable, and an earlier revision of this crate did support it. It
cost a positional code path, a lifetime on `QueryWriter`, and a class of error
the other drivers cannot raise. For a first version it is left out. Adding it
back is a breaking change, which is the right way round.

## What it refuses

Every one of these is raised before the database is touched.

| error | why |
| --- | --- |
| `Trailing`, `Fragment` | the fragment was not one complete expression |
| `SetOperation` | the outermost level is a `UNION`, so there is no single `SELECT` to filter -- wrap it in `SELECT * FROM (...) AS t` |
| `Grouped` | there is a `GROUP BY`, so a predicate could mean `WHERE` or `HAVING` and the fragment does not say which |
| `Arity` | the statement has a different number of placeholders than values bound |
| `Orphaned` | a value has no placeholder to bind to -- the base query skips a number, or `limit()` replaced a `LIMIT` that held one |
| `Sort` | an `order_by` value did not parse |
| `Filter` | a `filter` value did not parse, or asked for something with no meaning as SQL |
| `Field` | a request named a field the column map does not have |
| `Unresolved` | a `Sort` reached the query still holding field names |
| `NotQuery`, `Query` | the base SQL was not a single parseable query |

## Status

A spike. `QueryWriter` and `QueryBuilder` are here, with AIP-132 ordering.
Still to come: keyset cursors (`seek_by`) and CEL filters (`filter`) on the
builder. Filtering into a named CTE rather than the outermost `SELECT` is not
planned.

[sqlx]: https://github.com/launchbadge/sqlx
