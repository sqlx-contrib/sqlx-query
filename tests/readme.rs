//! Compile-checks the README's example against the real API.
#![cfg(all(feature = "cel", feature = "postgres"))]

use sqlx::Postgres;
use sqlx_query::{Cursor, Filter, QueryTemplate, Sort};

struct Request {
    filter: String,
    order_by: String,
    page_token: String,
}
#[allow(dead_code)]
#[derive(sqlx_query::Schema)]
#[schema(rename_all = "camelCase")]
struct Volume {
    #[schema(key)]
    id: i64,
    title: String,
    read_count: i64,
}

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

    let volumes = QueryTemplate::<Postgres>::parse(
        "SELECT id, title, read_count
       FROM volumes
      WHERE tenant_id = $1
        /* AND query.predicate */
      /* ORDER BY query.order */
      LIMIT $2",
    )?;

    let filter = Filter::parse(&request.filter)?;
    let sort = Sort::parse(&request.order_by)?.asc("id");
    let cursor = Cursor::parse(&request.page_token)?;

    cursor.validate(&sort)?;

    let query = volumes
        .splice()
        .bind(tenant_id)
        .bind(page_size)
        .fill("predicate", &filter.to_fragment(Volume::schema())?)
        .fill("predicate", &cursor.to_fragment(Volume::schema())?)
        .fill("order", &sort.to_fragment(Volume::schema())?);

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

    let second = volumes
        .splice()
        .bind(tenant_id)
        .bind(page_size)
        .fill("predicate", &filter.to_fragment(Volume::schema())?)
        .fill("predicate", &resumed.to_fragment(Volume::schema())?)
        .fill("order", &sort.to_fragment(Volume::schema())?);

    assert_eq!(
        second.sql(),
        "SELECT id, title, read_count\n       FROM volumes\n      WHERE tenant_id = $1\n        \
         AND (\"read_count\" > $3 AND \"title\" LIKE $4 ESCAPE '!') \
         AND ((\"title\" < $5) OR (\"title\" = $6 AND \"id\" > $7))\n      \
         ORDER BY \"title\" DESC, \"id\" ASC\n      LIMIT $2"
    );

    Ok(())
}
