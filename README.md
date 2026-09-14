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
    .order_by("title desc")
    .limit(50);

assert_eq!(
    writer.sql()?,
    "SELECT id, title, read_count FROM volumes \
     WHERE tenant_id = $1 AND read_count > 100 \
     ORDER BY title DESC, id LIMIT 50",
);

let rows = writer.build_as::<Volume>()?.fetch_all(&pool).await?;
```

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

## Placeholder numbering is not the same everywhere

PostgreSQL's `$N` names the *N*th bound value, so a fragment spliced ahead of a
`$2` leaves it pointing at the same thing:

```
base      SELECT id FROM users WHERE tenant_id = $1 LIMIT $2
fragment  role = $1
result    SELECT id FROM users WHERE tenant_id = $1 AND role = $3 LIMIT $2
```

MySQL's and SQLite's `?` names the *N*th placeholder *in the text*, so the same
rewrite would shift what `LIMIT ?` binds. That is `Error::Positional` rather
than a query that runs and is wrong.

Values are bound in the order the placeholders claim them: the base query's
first, then each fragment's, in the order the fragments were added.

## What it refuses

Every one of these is raised before the database is touched.

| error | why |
| --- | --- |
| `Trailing`, `Fragment` | the fragment was not one complete expression |
| `SetOperation` | the outermost level is a `UNION`, so there is no single `SELECT` to filter -- wrap it in `SELECT * FROM (...) AS t` |
| `Grouped` | there is a `GROUP BY`, so a predicate could mean `WHERE` or `HAVING` and the fragment does not say which |
| `Positional` | the rewrite would rebind a `?` to the wrong value |
| `Orphaned` | the rewrite removed a placeholder that still has a value bound to it |
| `NotQuery`, `Query` | the base SQL was not a single parseable query |

## Status

A spike. The API is `QueryWriter` and nothing else yet; the AIP-shaped layer
above it -- CEL filters, AIP-132 ordering, keyset cursors -- is not here, and
neither is targeting a filter at a named CTE rather than the outermost
`SELECT`.

[sqlx]: https://github.com/launchbadge/sqlx
