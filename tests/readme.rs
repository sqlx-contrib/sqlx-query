//! Compile-checks the README's example against the real API.
#![cfg(all(feature = "cel", feature = "postgres"))]

use sqlx::Postgres;
use std::sync::LazyLock;

use sqlx_query::{Column, ColumnType, Cursor, Filter, QueryMapping, QueryTemplate, Sort};

struct Request {
    filter: String,
    order_by: String,
    page_token: String,
}
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

/// What this query exposes, under what public name. A path not named here is
/// rejected, not passed through.
static MAPPING: LazyLock<QueryMapping> = LazyLock::new(|| {
    QueryMapping::new()
        .key("id", ColumnType::Int)
        .column("title", ColumnType::Text)
        .add("readCount", Column::new("read_count", ColumnType::Int))
});

/// A token issued by an earlier build for `title desc, id asc` at
/// ("Dune", 4711). Pinned so a change to the encoding shows up here: clients
/// persist these across deploys, and drifting silently would break live
/// pagination rather than fail loudly.
const TOKEN: &str = "AQEAAAAFdGl0bGUDAAAABER1bmUAAAAAAmlkAQAAAAAAABJn";

#[test]
fn the_readme_example_is_real() -> Result<(), sqlx_query::Error> {
    let request = Request {
        filter: "readCount > 100 && title.startsWith('D')".into(),
        order_by: "title desc".into(),
        page_token: String::new(),
    };
    let (tenant_id, page_size) = (7_i64, 50_i64);

    let filter = Filter::parse(&request.filter)?;
    let sort = Sort::parse(&request.order_by)?.asc("id");
    let cursor = Cursor::parse(&request.page_token)?;

    cursor.validate(&sort)?;

    let query = VOLUMES
        .builder(&*MAPPING)
        .bind(tenant_id)
        .bind(page_size)
        .fill("filter", &filter)
        .fill("filter", &cursor)
        .fill("order", &sort);

    assert_eq!(
        query.sql(),
        "SELECT id, title, read_count\n       FROM volumes\n      WHERE tenant_id = $1\n        \
         AND (\"read_count\" > $3 AND \"title\" LIKE $4 ESCAPE '!')\n      \
         ORDER BY \"title\" DESC, \"id\" ASC\n      LIMIT $2"
    );
    let _ = query.build()?;

    // Second page. Minting needs a live row, so `tests/sqlite.rs` covers that
    // end to end. This token was issued by an earlier build for
    // `title desc, id asc` at ("Dune", 4711), so it doubles as a guard on the
    // format: clients keep these across deploys, and a silent change would
    // break live pagination.
    let resumed = Cursor::parse(TOKEN)?;
    resumed.validate(&sort)?;

    let second = VOLUMES
        .builder(&*MAPPING)
        .bind(tenant_id)
        .bind(page_size)
        .fill("filter", &filter)
        .fill("filter", &resumed)
        .fill("order", &sort);

    assert_eq!(
        second.sql(),
        "SELECT id, title, read_count\n       FROM volumes\n      WHERE tenant_id = $1\n        \
         AND (\"read_count\" > $3 AND \"title\" LIKE $4 ESCAPE '!') \
         AND ((\"title\" < $5) OR (\"title\" = $6 AND \"id\" > $7))\n      \
         ORDER BY \"title\" DESC, \"id\" ASC\n      LIMIT $2"
    );

    Ok(())
}
