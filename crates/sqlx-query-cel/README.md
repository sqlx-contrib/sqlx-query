# sqlx-query-cel

Reads an [AIP-160](https://google.aip.dev/160) `filter` written in
[CEL](https://github.com/google/cel-spec) into a `WhereClause` that
[`sqlx-query`](https://crates.io/crates/sqlx-query) splices into a query you
already wrote.

```rust
let filter = FilterClause::parse("rank > 10 && name != 'root'")?
    .resolve(&columns)?;      // fail-closed: an unknown field is an error

query.push_where(filter);
//   -> (rank) > ($1) AND (name) != ($2)
```

Every literal becomes a bind value; no request text reaches the SQL. Fields
are renamed through an allow-list, so a request can only filter on columns you
offered by name.

Accepts the comparison-and-boolean part of CEL: `&&`, `||`, `!`, the six
comparisons, `in` over a list, arithmetic and literals, plus the string methods
`startsWith`, `endsWith` and `contains`, which read as `LIKE`. Macros,
comprehensions, other function calls, maps and structs are refused — they have
no reading as a `WHERE` clause, and guessing one would invent SQL the caller
didn't ask for.

The dependency runs one way: `sqlx-query` never knows about CEL, so a
different filter syntax is a new crate rather than a fork.

**Full documentation:** <https://github.com/sqlx-contrib/sqlx-query>

## License

[MIT](https://github.com/sqlx-contrib/sqlx-query/blob/main/LICENSE)
