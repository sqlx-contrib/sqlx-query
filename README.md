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

## Placeholders are numbered, then written back out

Whatever the driver spells them as, placeholders are numbered on the way in and
written back in that driver's form at the end. In between there is one kind of
placeholder and the rewrite is arithmetic: a fragment's `$1` becomes `$3`
because two values were claimed before it.

Values are given in the order the placeholders claim them -- the base query's
first, then each fragment's. What comes out depends only on what the driver can
say.

PostgreSQL's `$N` and SQLite's `?N` each name the value they want, so the
numbering survives to the wire and a fragment spliced into the middle disturbs
nothing:

```
base      SELECT id FROM users WHERE tenant_id = ? LIMIT ?
fragment  role = ?
sqlite    SELECT id FROM users WHERE tenant_id = ?1 AND role = ?3 LIMIT ?2
postgres  SELECT id FROM users WHERE tenant_id = $1 AND role = $3 LIMIT $2
```

MySQL has no numbered form -- `?1` is a syntax error and `$1` is read as a
column name -- so its placeholders go out bare and take a value each, in the
order they appear. The values are sent in that order rather than the order they
were given:

```
mysql     SELECT id FROM users WHERE tenant_id = ? AND role = ? LIMIT ?
given     tenant, limit, role
sent      tenant, role, limit
```

The one thing a bare `?` cannot express is a value wanted twice. `$1` or `?1`
used in two places is ordinary elsewhere; on MySQL it is `Error::Positional`.

## What it refuses

Every one of these is raised before the database is touched.

| error | why |
| --- | --- |
| `Trailing`, `Fragment` | the fragment was not one complete expression |
| `SetOperation` | the outermost level is a `UNION`, so there is no single `SELECT` to filter -- wrap it in `SELECT * FROM (...) AS t` |
| `Grouped` | there is a `GROUP BY`, so a predicate could mean `WHERE` or `HAVING` and the fragment does not say which |
| `Positional` | one value is wanted by two placeholders, which MySQL's bare `?` cannot express |
| `Orphaned` | the rewrite removed a placeholder that still has a value bound to it |
| `Unbound` | fewer values were bound than the statement has placeholders |
| `NotQuery`, `Query` | the base SQL was not a single parseable query |

## Status

A spike. The API is `QueryWriter` and nothing else yet; the AIP-shaped layer
above it -- CEL filters, AIP-132 ordering, keyset cursors -- is not here, and
neither is targeting a filter at a named CTE rather than the outermost
`SELECT`.

[sqlx]: https://github.com/launchbadge/sqlx
