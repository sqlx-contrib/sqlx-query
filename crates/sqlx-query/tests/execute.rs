//! Tests that run a composed statement against a real driver.
//!
//! Everything else in this repo compares rendered SQL, which is
//! structurally incapable of catching a bind-order bug: the text can be
//! right and the values still arrive against the wrong placeholders. On a
//! `?` dialect that is exactly the failure mode, because a marker is bound
//! by where it sits rather than by a number it carries.
//!
//! SQLite runs in memory, so these need no server and no feature gate.
//! PostgreSQL and MySQL live in `postgres.rs`/`mysql.rs`, behind the URLs
//! the Dev Container sets.

#![cfg(feature = "sqlite")]

use sqlx::{Row, SqlitePool};
use sqlx_query::{Cursor, OrderByClause, Pager, QueryComposer, Value, WhereClause};

const SCHEMA: &str = "
    CREATE TABLE orders (
        id         INTEGER PRIMARY KEY,
        tenant_id  TEXT    NOT NULL,
        status     TEXT    NOT NULL,
        rank       INTEGER NOT NULL,
        receipt    BLOB
    )
";

/// The layout the README documents and sqlc generates: a slot *ahead* of
/// the base query's own markers, and a `LIMIT` behind them.
const LIST_ORDERS: &str = "
    SELECT id, tenant_id, status, rank, receipt
      FROM orders
     WHERE /* query.where AND */ tenant_id = ?
     ORDER BY /* query.order_by , */ id
     LIMIT ?
";

async fn seed() -> SqlitePool {
    let pool = SqlitePool::connect(":memory:")
        .await
        .expect("in-memory sqlite");
    sqlx::raw_sql(SCHEMA).execute(&pool).await.expect("schema");

    for (id, tenant, status, rank) in [
        (1, "acme", "SHIPPED", 10),
        (2, "acme", "PENDING", 20),
        (3, "acme", "SHIPPED", 30),
        (4, "other", "SHIPPED", 40),
    ] {
        sqlx::query("INSERT INTO orders (id, tenant_id, status, rank) VALUES (?, ?, ?, ?)")
            .bind(id)
            .bind(tenant)
            .bind(status)
            .bind(rank)
            .execute(&pool)
            .await
            .expect("insert");
    }
    pool
}

/// The test the whole `?`-numbering change exists for.
///
/// The filter's value belongs to the slot, which sits before `tenant_id`'s
/// marker; `LIMIT`'s belongs after. Bind them in declaration order instead
/// of textual order — which is what this crate did until recently — and
/// `tenant_id` is compared against `20`, `rank` against `"acme"`, and the
/// query returns nothing rather than failing loudly.
#[tokio::test]
async fn a_slot_ahead_of_the_base_markers_binds_in_textual_order() {
    let pool = seed().await;

    let mut query = QueryComposer::<sqlx::Sqlite>::new(LIST_ORDERS);
    query
        .bind_value("acme")
        .bind_value(10i64)
        .push_where(WhereClause::new("rank > $1").bind_value(20i64));

    let rows = query
        .build()
        .expect("composes")
        .fetch_all(&pool)
        .await
        .expect("runs");

    let ids: Vec<i64> = rows.iter().map(|row| row.get("id")).collect();
    assert_eq!(ids, vec![3], "only acme's order with rank > 20");
}

/// A cursor references each boundary value twice — `(rank < $1) OR (rank =
/// $1 AND id > $2)`. On a `?` dialect that has to become two markers and
/// two copies of the value; one copy would shift every later binding by
/// one and silently compare the wrong columns.
#[tokio::test]
async fn a_cursor_value_referenced_twice_binds_twice() {
    let pool = seed().await;

    let order_by = OrderByClause::default().desc("rank").asc("id");

    let mut first = QueryComposer::<sqlx::Sqlite>::new(LIST_ORDERS);
    first
        .bind_value("acme")
        .bind_value(2i64)
        .push_order_by(order_by.clone());
    let rows = first
        .build()
        .expect("composes")
        .fetch_all(&pool)
        .await
        .expect("runs");

    let ids: Vec<i64> = rows.iter().map(|row| row.get("id")).collect();
    assert_eq!(ids, vec![3, 2], "rank descending, page of two");

    // Page two, seeking past the last row of page one.
    let cursor = Cursor::new(order_by)
        .after_row(rows.last().expect("a row"))
        .expect("cursor from the last row");

    let mut second = QueryComposer::<sqlx::Sqlite>::new(LIST_ORDERS);
    second
        .bind_value("acme")
        .bind_value(2i64)
        .with_cursor(cursor);
    let rows = second
        .build()
        .expect("composes")
        .fetch_all(&pool)
        .await
        .expect("runs");

    let ids: Vec<i64> = rows.iter().map(|row| row.get("id")).collect();
    assert_eq!(ids, vec![1], "the page after rank 20 is rank 10");
}

/// A page token survives the round trip through base64 and back into a
/// query that runs.
#[tokio::test]
async fn a_page_token_round_trips_into_a_running_query() {
    let pool = seed().await;

    let order_by = OrderByClause::default().asc("id");
    let mut first = QueryComposer::<sqlx::Sqlite>::new(LIST_ORDERS);
    first
        .bind_value("acme")
        .bind_value(1i64)
        .push_order_by(order_by.clone());
    let rows = first
        .build()
        .expect("composes")
        .fetch_all(&pool)
        .await
        .expect("runs");

    let token = Cursor::new(order_by)
        .after_row(rows.last().expect("a row"))
        .expect("cursor")
        .encode();

    let mut second = QueryComposer::<sqlx::Sqlite>::new(LIST_ORDERS);
    second
        .bind_value("acme")
        .bind_value(1i64)
        .with_cursor(Cursor::parse(&token).expect("token parses"));
    let rows = second
        .build()
        .expect("composes")
        .fetch_all(&pool)
        .await
        .expect("runs");

    let ids: Vec<i64> = rows.iter().map(|row| row.get("id")).collect();
    assert_eq!(ids, vec![2], "the row after id 1");
}

/// `Value::Bytes` reaches a `BLOB` column and comes back.
#[tokio::test]
async fn bytes_bind_and_decode() {
    let pool = seed().await;
    let receipt = vec![0xDE_u8, 0xAD, 0xBE, 0xEF];

    sqlx::query("UPDATE orders SET receipt = ? WHERE id = ?")
        .bind(receipt.clone())
        .bind(1i64)
        .execute(&pool)
        .await
        .expect("update");

    let mut query = QueryComposer::<sqlx::Sqlite>::new(LIST_ORDERS);
    query
        .bind_value("acme")
        .bind_value(10i64)
        .push_where(WhereClause::new("receipt = $1").bind_value(receipt.clone()));

    let rows = query
        .build()
        .expect("composes")
        .fetch_all(&pool)
        .await
        .expect("runs");

    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].get::<Vec<u8>, _>("receipt"), receipt);
}

/// `bind` erases the type, so this is the only check that the erased path
/// encodes at all — the string comparison tests can't see past the box.
#[tokio::test]
async fn an_erased_bind_encodes() {
    let pool = seed().await;

    let mut query = QueryComposer::<sqlx::Sqlite>::new(LIST_ORDERS);
    query
        .bind("acme".to_owned()) // through Bindable, not Value
        .bind(10i64)
        .push_where(WhereClause::new("rank > $1").bind_value(20i64));

    let rows = query
        .build()
        .expect("composes")
        .fetch_all(&pool)
        .await
        .expect("runs");

    let ids: Vec<i64> = rows.iter().map(|row| row.get("id")).collect();
    assert_eq!(ids, vec![3]);
}

/// A composed statement with no clauses is still a statement that runs —
/// the slots are comments, so an empty filter is not a special case.
#[tokio::test]
async fn a_query_with_every_slot_empty_still_runs() {
    let pool = seed().await;

    let mut query = QueryComposer::<sqlx::Sqlite>::new(LIST_ORDERS);
    query.bind_value("acme").bind_value(10i64);

    let rows = query
        .build()
        .expect("composes")
        .fetch_all(&pool)
        .await
        .expect("runs");

    let ids: Vec<i64> = rows.iter().map(|row| row.get("id")).collect();
    assert_eq!(ids, vec![1, 2, 3]);
}

/// The values a `?` statement reports are per *occurrence*, and in the
/// order the driver will consume them.
#[tokio::test]
async fn reported_arguments_match_what_the_driver_receives() {
    let mut query = QueryComposer::<sqlx::Sqlite>::new(LIST_ORDERS);
    query
        .bind_value("acme")
        .bind_value(10i64)
        .push_where(WhereClause::new("rank > $1").bind_value(20i64));

    let statement = query.compose().expect("composes");
    let values: Vec<Option<&Value>> = statement
        .arguments()
        .iter()
        .map(sqlx_query::QueryArgument::value)
        .collect();

    assert_eq!(
        values,
        vec![
            Some(&Value::Int(20)),
            Some(&Value::String("acme".into())),
            Some(&Value::Int(10)),
        ],
        "filter first: its slot precedes the base query's own markers"
    );
}

/// The ids `query` returns, in order.
async fn ids(pool: &SqlitePool, query: QueryComposer<sqlx::Sqlite>) -> Vec<i64> {
    query
        .build()
        .expect("composes")
        .fetch_all(pool)
        .await
        .expect("runs")
        .iter()
        .map(|row| row.get::<i64, _>("id"))
        .collect()
}

/// A fragment with a top-level `OR` is spliced in parenthesised, so the base
/// query's own condition still applies to every row it matches. Bare, `AND`
/// would bind tighter: `status = ? OR rank > ? AND tenant_id = ?` returns
/// another tenant's shipped order.
#[tokio::test]
async fn a_top_level_or_stays_inside_the_base_condition() {
    let pool = seed().await;

    let mut query = QueryComposer::<sqlx::Sqlite>::new(LIST_ORDERS);
    query.bind_value("acme").bind_value(10i64).push_where(
        WhereClause::new("status = $1 OR rank > $2")
            .bind_value("SHIPPED")
            .bind_value(35i64),
    );

    assert_eq!(
        ids(&pool, query).await,
        [1, 3],
        "acme's shipped orders only"
    );
}

/// A cursor is an `OR` of `AND`s, so a page after the first is the same case:
/// it has to stay inside the base condition, or paging walks into another
/// tenant's rows.
#[tokio::test]
async fn a_cursor_page_stays_inside_the_base_condition() {
    let pool = seed().await;
    let order_by = OrderByClause::default().asc("rank").asc("id");
    let cursor = Cursor::new(order_by)
        .after(vec![Value::Int(20), Value::Int(2)])
        .expect("two keys, two values");

    let mut query = QueryComposer::<sqlx::Sqlite>::new(LIST_ORDERS);
    query
        .bind_value("acme")
        .bind_value(10i64)
        .with_cursor(cursor);

    assert_eq!(
        ids(&pool, query).await,
        [3],
        "the page after acme's rank 20"
    );
}

/// Page through acme's orders `size` at a time by rank, carrying each
/// page's cursor to the next as a token -- the way it travels between
/// requests, from the empty token to the empty token -- and return the ids
/// of every page.
async fn pages(pool: &SqlitePool, size: usize) -> Vec<Vec<i64>> {
    let order_by = OrderByClause::default().asc("rank").asc("id");
    let pager = Pager::new(Cursor::new(order_by.clone()), size);
    let (mut seen, mut token) = (Vec::new(), String::new());

    // Bounded, so a cursor that never runs out fails instead of hanging.
    for _ in 0..10 {
        let mut query = QueryComposer::<sqlx::Sqlite>::new(LIST_ORDERS);
        query
            .bind_value("acme")
            .bind_value(pager.limit())
            .push_order_by(order_by.clone())
            .with_cursor(Cursor::parse(&token).expect("the token parses"));

        let rows = query
            .build()
            .expect("composes")
            .fetch_all(pool)
            .await
            .expect("runs");
        let page = pager.next_page(rows).expect("the row carries every key");

        seen.push(page.rows.iter().map(|row| row.get("id")).collect());
        token = page.cursor.encode();
        if token.is_empty() {
            return seen;
        }
    }
    panic!("still paging after ten pages: {seen:?}");
}

/// Every row once, in order, and the last page is the one with no cursor.
#[tokio::test]
async fn a_pager_pages_through_every_row_once() {
    let pool = seed().await;

    assert_eq!(pages(&pool, 2).await, [vec![1, 2], vec![3]]);
    assert_eq!(pages(&pool, 1).await, [vec![1], vec![2], vec![3]]);
}

/// A page that exactly fills the size is the last: the extra row the pager
/// asks for is what says there is another, and it did not come back.
#[tokio::test]
async fn a_page_that_fills_exactly_has_no_cursor() {
    let pool = seed().await;

    assert_eq!(pages(&pool, 3).await, [vec![1, 2, 3]]);
}
