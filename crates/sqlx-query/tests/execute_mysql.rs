//! The MySQL half of the live-driver tests.
//!
//! Skipped when `SQLX_QUERY_MYSQL_URL` is unset, so `make test` stays
//! offline.
//!
//! What these check that SQLite can't: MySQL quotes identifiers with
//! backticks and starts a line comment with `#`. The scanner has to skip
//! both, because a `?` inside either is text — and if it doesn't, the
//! statement binds one value too many and the driver rejects it. That's
//! the per-dialect lexing this crate added, and only MySQL can prove it.

#![cfg(feature = "mysql")]

use sqlx::{MySqlPool, Row};
use sqlx_query::{Cursor, OrderByClause, QueryComposer, WhereClause};

async fn pool() -> Option<MySqlPool> {
    let url = std::env::var("SQLX_QUERY_MYSQL_URL").ok()?;
    Some(MySqlPool::connect(&url).await.expect("mysql connects"))
}

async fn table(pool: &MySqlPool, name: &str, columns: &str) {
    // `name` and `columns` are literals in this file, never input.
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!("DROP TABLE IF EXISTS {name}")))
        .execute(pool)
        .await
        .expect("drop");
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
        "CREATE TABLE {name} ({columns})"
    )))
    .execute(pool)
    .await
    .expect("create");
}

macro_rules! pool_or_skip {
    () => {
        match pool().await {
            Some(pool) => pool,
            None => {
                eprintln!("skipped: SQLX_QUERY_MYSQL_URL is unset");
                return;
            }
        }
    };
}

async fn seed(pool: &MySqlPool, name: &str) {
    table(
        pool,
        name,
        "id bigint primary key, tenant_id varchar(32) not null, score bigint not null",
    )
    .await;

    for (id, tenant, score) in [(1i64, "acme", 10i64), (2, "acme", 30), (3, "other", 40)] {
        sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
            "INSERT INTO {name} VALUES ({id}, '{tenant}', {score})"
        )))
        .execute(pool)
        .await
        .expect("insert");
    }
}

/// The same ordering check as SQLite, on a second `?` driver: a slot ahead
/// of the base query's own markers binds ahead of them.
#[tokio::test]
async fn a_slot_ahead_of_the_base_markers_binds_in_textual_order() {
    let pool = pool_or_skip!();
    seed(&pool, "my_order_test").await;

    let sql = "SELECT id FROM my_order_test \
               WHERE /* query.where AND */ tenant_id = ? \
               ORDER BY /* query.order_by , */ id LIMIT ?";
    let mut query = QueryComposer::<sqlx::MySql>::new(sql);
    query
        .bind_value("acme")
        .bind_value(10i64)
        .push_where(WhereClause::new("score > $1").bind_value(20i64));

    let rows = query
        .build()
        .expect("composes")
        .fetch_all(&pool)
        .await
        .expect("runs");

    let ids: Vec<i64> = rows.iter().map(|row| row.get("id")).collect();
    assert_eq!(ids, vec![2]);
}

/// A `?` inside a backtick-quoted identifier is part of the identifier.
///
/// Read it as a placeholder and the statement claims one more parameter
/// than the composer supplies, which MySQL rejects outright — so this
/// passing is the scanner's backtick handling working, not a coincidence.
#[tokio::test]
async fn a_question_mark_inside_a_backtick_identifier_is_not_a_placeholder() {
    let pool = pool_or_skip!();
    table(
        &pool,
        "my_backtick_test",
        "`why?` bigint primary key, tenant_id varchar(32) not null",
    )
    .await;
    sqlx::raw_sql(sqlx::AssertSqlSafe(
        "INSERT INTO my_backtick_test VALUES (7, 'acme')".to_owned(),
    ))
    .execute(&pool)
    .await
    .expect("insert");

    let sql = "SELECT `why?` AS id FROM my_backtick_test \
               WHERE /* query.where AND */ tenant_id = ?";
    let mut query = QueryComposer::<sqlx::MySql>::new(sql);
    query.bind_value("acme");

    let rows = query
        .build()
        .expect("composes")
        .fetch_all(&pool)
        .await
        .expect("runs");

    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].get::<i64, _>("id"), 7);
}

/// Same, for MySQL's `#` line comment — which no other dialect here has.
#[tokio::test]
async fn a_question_mark_inside_a_hash_comment_is_not_a_placeholder() {
    let pool = pool_or_skip!();
    seed(&pool, "my_hash_test").await;

    let sql = "SELECT id FROM my_hash_test # is ? a placeholder\n\
               WHERE /* query.where AND */ tenant_id = ?";
    let mut query = QueryComposer::<sqlx::MySql>::new(sql);
    query.bind_value("acme");

    let rows = query
        .build()
        .expect("composes")
        .fetch_all(&pool)
        .await
        .expect("runs");

    let ids: Vec<i64> = rows.iter().map(|row| row.get("id")).collect();
    assert_eq!(ids, vec![1, 2]);
}

/// A cursor's repeated boundary value becomes two markers and two copies,
/// same as SQLite — checked here because MySQL counts parameters strictly
/// and errors rather than silently misbinding.
#[tokio::test]
async fn a_cursor_value_referenced_twice_binds_twice() {
    let pool = pool_or_skip!();
    seed(&pool, "my_cursor_test").await;

    let order_by = OrderByClause::default().desc("score").asc("id");
    let sql = "SELECT id, score FROM my_cursor_test \
               WHERE /* query.where AND */ tenant_id = ? \
               ORDER BY /* query.order_by , */ id";

    let mut first = QueryComposer::<sqlx::MySql>::new(sql);
    first.bind_value("acme").push_order_by(order_by.clone());
    let rows = first
        .build()
        .expect("composes")
        .fetch_all(&pool)
        .await
        .expect("runs");
    assert_eq!(
        rows.iter()
            .map(|r| r.get::<i64, _>("id"))
            .collect::<Vec<_>>(),
        vec![2, 1]
    );

    let cursor = Cursor::new(order_by)
        .after_row(&rows[0])
        .expect("cursor from the first row");

    let mut second = QueryComposer::<sqlx::MySql>::new(sql);
    second.bind_value("acme").with_cursor(cursor);
    let rows = second
        .build()
        .expect("composes")
        .fetch_all(&pool)
        .await
        .expect("runs");

    assert_eq!(
        rows.iter()
            .map(|r| r.get::<i64, _>("id"))
            .collect::<Vec<_>>(),
        vec![1],
        "everything after score 30"
    );
}
