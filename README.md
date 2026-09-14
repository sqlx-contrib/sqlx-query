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
    .filter_by("read_count > 100")
    .sort_by("title desc")
    .limit(50);

assert_eq!(
    writer.sql()?,
    "SELECT id, title, read_count FROM volumes \
     WHERE tenant_id = $1 AND read_count > 100 \
     ORDER BY title DESC, id LIMIT 50",
);

let rows = writer.build_as::<Volume>()?.fetch_all(&pool).await?;
```

## Two layers

`QueryWriter` takes SQL fragments, which you wrote and therefore vouch for.
`QueryBuilder` takes what a client asked for, already parsed and checked
against a map of the fields you chose to offer.

```rust
use sqlx::Postgres;
use sqlx_query::{QueryBuilder, Sort};

// What a request may order by, and the column each name means. Anything not
// here is refused rather than passed through.
let columns = HashMap::from([("title", "title"), ("readCount", "read_count"), ("id", "id")]);

let sort = Sort::parse(&request.order_by)?   // AIP-132: "readCount desc"
    .asc("id")                               // a tiebreaker, so the order is total
    .resolve(&columns)?;                     // fields become columns, or are refused

let mut query = QueryBuilder::<Postgres>::new(
    "SELECT id, title, read_count FROM volumes WHERE tenant_id = $1",
)?;
query.bind(tenant).sort_by(&sort).limit(50);

let rows = query.build_as::<Volume>()?.fetch_all(&pool).await?;
```

```sql
SELECT id, title, read_count FROM volumes WHERE tenant_id = $1
ORDER BY "read_count" DESC, "id" ASC
LIMIT 50
```

The two are separate types because an AIP `order_by` value and a SQL
`ORDER BY` fragment look identical -- `"title desc"` is both -- so one type
offering both would let a client's string reach the unchecked path with nothing
at the call site looking wrong.

| | takes | checked against the map |
| --- | --- | --- |
| `QueryWriter` | `filter_by("role = 'admin'")` | no -- you wrote it |
| `QueryBuilder` | `sort_by(&sort)` | yes |

A `Sort` that was never resolved is refused rather than written into the query,
so forgetting the step cannot quietly skip the allowlist.

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

Exactly one expression. `filter_by` parses its argument and then insists the
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
| `Field` | a request named a field the column map does not have |
| `Unresolved` | a `Sort` reached the query still holding field names |
| `NotQuery`, `Query` | the base SQL was not a single parseable query |

## Status

A spike. `QueryWriter` and `QueryBuilder` are here, with AIP-132 ordering.
Still to come: keyset cursors (`seek_by`) and CEL filters (`filter_by`) on the
builder. Filtering into a named CTE rather than the outermost `SELECT` is not
planned.

[sqlx]: https://github.com/launchbadge/sqlx
