# sqlx-query

Add filtering, ordering and keyset pagination to a SQL query you already
wrote. No query builder, no DSL — your SQL keeps its shape, and the pieces get
spliced into comments you left for them.

```sql
SELECT id, name, rank, created_at
  FROM users
 WHERE /* query.where AND */ tenant_id = $1
 ORDER BY /* query.order_by , */ id
 LIMIT $2
```

That query runs as-is: the slots are comments, so an empty filter is not a
special case, it is just a comment nobody replaced.

```rust
let mut query = QueryComposer::<Postgres>::new(LIST_USERS);
query
    .bind_value(tenant_id)
    .bind_value(50i64)
    .push_where(filter)
    .push_order_by(order_by);

let users = query.build()?.fetch_all(&pool).await?;
```

This crate is the composer, the clause types and the cursor. It has no filter
language: anything `WHERE`-shaped converts *into* a `WhereClause`, so
[`sqlx-query-cel`](https://crates.io/crates/sqlx-query-cel) can read an
AIP-160 filter without this crate depending on CEL.

Works with PostgreSQL, MySQL and SQLite, each lexed by its own rules.

**Full documentation, limitations and rationale:**
<https://github.com/sqlx-contrib/sqlx-query>

## License

[MIT](https://github.com/sqlx-contrib/sqlx-query/blob/main/LICENSE)
