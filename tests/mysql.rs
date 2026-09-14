//! The replay path, against a real MySQL server.
//!
//! MySQL is the only driver whose placeholder is bound by position and cannot
//! be numbered -- `?1` is a syntax error and `$1` parses as a column name --
//! so it is the only one whose values have to be sent in the order the
//! finished statement renders rather than the order they were given. That
//! reordering is invisible in the SQL, so only a server can tell whether it
//! was right.
//!
//! Needs a server: set `SQLX_QUERY_MYSQL_URL`. Without it these skip, so a
//! machine with no Docker still runs green -- and covers less.

#![cfg(feature = "mysql")]

use sqlx::{AssertSqlSafe, MySqlPool, Row};
use sqlx_query::QueryWriter;

/// Connects, or returns `None` so the test reports itself as skipped rather
/// than failing on a machine that was never going to have a server.
async fn pool() -> Option<MySqlPool> {
    let url = std::env::var("SQLX_QUERY_MYSQL_URL").ok()?;
    Some(
        MySqlPool::connect(&url)
            .await
            .expect("SQLX_QUERY_MYSQL_URL is set but unreachable"),
    )
}

/// A table per test, because these may run against a shared server and
/// concurrently with each other.
async fn seed(pool: &MySqlPool, table: &str) {
    sqlx::query(AssertSqlSafe(format!("DROP TABLE IF EXISTS {table}")))
        .execute(pool)
        .await
        .unwrap();
    sqlx::query(AssertSqlSafe(format!(
        "CREATE TABLE {table} (
            id        BIGINT PRIMARY KEY,
            tenant_id BIGINT NOT NULL,
            name      VARCHAR(32) NOT NULL,
            role      VARCHAR(32) NOT NULL
        )"
    )))
    .execute(pool)
    .await
    .unwrap();

    for (id, tenant, name, role) in [
        (1, 1, "ada", "admin"),
        (2, 1, "grace", "admin"),
        (3, 1, "alan", "member"),
        (4, 2, "edsger", "admin"),
    ] {
        sqlx::query(AssertSqlSafe(format!(
            "INSERT INTO {table} (id, tenant_id, name, role) VALUES (?, ?, ?, ?)"
        )))
        .bind(id)
        .bind(tenant)
        .bind(name)
        .bind(role)
        .execute(pool)
        .await
        .unwrap();
    }
}

/// The filter renders between `tenant_id = ?` and `LIMIT ?`, so the values are
/// wanted as tenant, role, limit -- but were given as tenant, limit, role,
/// because a fragment's values follow the base query's. Sending them as given
/// would put the page size on `role` and the string `admin` on `LIMIT`.
#[tokio::test]
async fn values_are_replayed_in_the_order_the_statement_wants() {
    let Some(pool) = pool().await else { return };
    seed(&pool, "replay_order").await;

    let mut writer = QueryWriter::<sqlx::MySql>::new(
        "SELECT id, name FROM replay_order WHERE tenant_id = ? ORDER BY id LIMIT ?",
    )
    .unwrap();
    writer
        .bind(1_i64) // base: tenant_id
        .bind(2_i64) // base: limit
        .filter_by("role = ?")
        .bind("admin"); // fragment

    assert_eq!(
        writer.sql().unwrap(),
        "SELECT id, name FROM replay_order WHERE tenant_id = ? AND role = ? ORDER BY id LIMIT ?"
    );

    let rows = writer.build().unwrap().fetch_all(&pool).await.unwrap();
    let names: Vec<String> = rows.iter().map(|r| r.get::<String, _>("name")).collect();

    assert_eq!(names, ["ada", "grace"]);
}

/// The same shape with a limit of one, so a mis-bound limit could not pass
/// unnoticed.
#[tokio::test]
async fn a_replayed_limit_still_limits() {
    let Some(pool) = pool().await else { return };
    seed(&pool, "replay_limit").await;

    let mut writer = QueryWriter::<sqlx::MySql>::new(
        "SELECT id, name FROM replay_limit WHERE tenant_id = ? ORDER BY id LIMIT ?",
    )
    .unwrap();
    writer
        .bind(1_i64)
        .bind(1_i64)
        .filter_by("role = ?")
        .bind("admin");

    let rows = writer.build().unwrap().fetch_all(&pool).await.unwrap();
    let names: Vec<String> = rows.iter().map(|r| r.get::<String, _>("name")).collect();

    assert_eq!(names, ["ada"]);
}

/// The `OR` grouping, executed. Without the parentheses this returns edsger
/// too, which is another tenant's row: a data leak rather than a syntax error.
#[tokio::test]
async fn an_or_in_the_base_query_keeps_its_grouping() {
    let Some(pool) = pool().await else { return };
    seed(&pool, "replay_or").await;

    let mut writer = QueryWriter::<sqlx::MySql>::new(
        "SELECT id, name FROM replay_or WHERE name = 'edsger' OR name = 'alan'",
    )
    .unwrap();
    writer.filter_by("tenant_id = ?").bind(1_i64);

    let rows = writer.build().unwrap().fetch_all(&pool).await.unwrap();
    let names: Vec<String> = rows.iter().map(|r| r.get::<String, _>("name")).collect();

    assert_eq!(names, ["alan"]);
}
