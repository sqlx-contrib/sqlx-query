//! The rewrite against a real database.
//!
//! SQLite in memory, so this needs no server and runs everywhere the rest of
//! the suite does. It is here to check the parts the string assertions cannot:
//! that the SQL is accepted, that the values land on the placeholders they
//! were meant for, and that `build` hands sqlx something it will execute.

#![cfg(feature = "sqlite")]

use sqlx::{Row, SqlitePool};
use sqlx_query::QueryWriter;

const SCHEMA: &str = "
    CREATE TABLE users (
        id        INTEGER PRIMARY KEY,
        tenant_id INTEGER NOT NULL,
        name      TEXT    NOT NULL,
        role      TEXT    NOT NULL
    )
";

async fn seed() -> SqlitePool {
    let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
    sqlx::query(SCHEMA).execute(&pool).await.unwrap();

    for (id, tenant, name, role) in [
        (1, 1, "ada", "admin"),
        (2, 1, "grace", "admin"),
        (3, 1, "alan", "member"),
        (4, 2, "edsger", "admin"),
    ] {
        sqlx::query("INSERT INTO users (id, tenant_id, name, role) VALUES (?, ?, ?, ?)")
            .bind(id)
            .bind(tenant)
            .bind(name)
            .bind(role)
            .execute(&pool)
            .await
            .unwrap();
    }

    pool
}

#[derive(sqlx::FromRow, Debug, PartialEq, Eq)]
struct User {
    id: i64,
    name: String,
}

/// The base query's value and the fragment's are bound in that order, and each
/// lands on its own placeholder -- the thing a renumbering bug would break
/// without any SQL error to show for it.
#[tokio::test]
async fn a_filter_binds_its_own_value() {
    let pool = seed().await;

    let mut writer =
        QueryWriter::<sqlx::Sqlite>::new("SELECT id, name FROM users WHERE tenant_id = ?").unwrap();
    writer.bind(1_i64).filter_by("role = ?").bind("admin");

    let users: Vec<User> = writer
        .build_as::<User>()
        .unwrap()
        .fetch_all(&pool)
        .await
        .unwrap();

    assert_eq!(
        users,
        vec![
            User {
                id: 1,
                name: "ada".into()
            },
            User {
                id: 2,
                name: "grace".into()
            },
        ]
    );
}

/// The case that decides whether replaying values is right.
///
/// The filter renders between `tenant_id = ?` and `LIMIT ?`, so the three
/// values are wanted in the order tenant, role, limit -- but they were given
/// as tenant, limit, role, because a fragment's values follow the base
/// query's. Binding in the order they were given would put the page size on
/// `role` and the string `admin` on `LIMIT`.
#[tokio::test]
async fn values_are_replayed_in_the_order_the_statement_wants() {
    let pool = seed().await;

    let mut writer = QueryWriter::<sqlx::Sqlite>::new(
        "SELECT id, name FROM users WHERE tenant_id = ? ORDER BY id LIMIT ?",
    )
    .unwrap();
    writer
        .bind(1_i64) // base: tenant_id
        .bind(2_i64) // base: limit
        .filter_by("role = ?")
        .bind("admin"); // fragment

    assert_eq!(
        writer.sql().unwrap(),
        "SELECT id, name FROM users WHERE tenant_id = ? AND role = ? ORDER BY id LIMIT ?"
    );

    let users: Vec<User> = writer
        .build_as::<User>()
        .unwrap()
        .fetch_all(&pool)
        .await
        .unwrap();

    // Tenant 1 has two admins and one member; the limit of 2 is not what
    // trimmed this, but a mis-bound limit would have.
    assert_eq!(
        users,
        vec![
            User {
                id: 1,
                name: "ada".into()
            },
            User {
                id: 2,
                name: "grace".into()
            },
        ]
    );
}

/// The same shape, with a limit small enough that binding it to the wrong
/// placeholder could not go unnoticed.
#[tokio::test]
async fn a_replayed_limit_still_limits() {
    let pool = seed().await;

    let mut writer = QueryWriter::<sqlx::Sqlite>::new(
        "SELECT id, name FROM users WHERE tenant_id = ? ORDER BY id LIMIT ?",
    )
    .unwrap();
    writer
        .bind(1_i64)
        .bind(1_i64)
        .filter_by("role = ?")
        .bind("admin");

    let users: Vec<User> = writer
        .build_as::<User>()
        .unwrap()
        .fetch_all(&pool)
        .await
        .unwrap();

    assert_eq!(
        users,
        vec![User {
            id: 1,
            name: "ada".into()
        }]
    );
}

/// Ordering by the fragment first, with the base `ORDER BY id` behind it.
#[tokio::test]
async fn ordering_puts_the_fragment_first() {
    let pool = seed().await;

    let mut writer =
        QueryWriter::<sqlx::Sqlite>::new("SELECT id, name FROM users ORDER BY id").unwrap();
    writer.order_by("name asc");

    let users: Vec<User> = writer
        .build_as::<User>()
        .unwrap()
        .fetch_all(&pool)
        .await
        .unwrap();

    let names: Vec<&str> = users.iter().map(|u| u.name.as_str()).collect();
    assert_eq!(names, ["ada", "alan", "edsger", "grace"]);
}

#[tokio::test]
async fn limit_applies() {
    let pool = seed().await;

    let mut writer =
        QueryWriter::<sqlx::Sqlite>::new("SELECT id, name FROM users ORDER BY id").unwrap();
    writer.limit(2);

    let users: Vec<User> = writer
        .build_as::<User>()
        .unwrap()
        .fetch_all(&pool)
        .await
        .unwrap();

    assert_eq!(users.len(), 2);
}

/// The `OR` case, executed rather than compared as text. Without the
/// parentheses this returns every admin in every tenant, which is a data leak
/// and not a syntax error -- nothing in the SQL would look wrong.
#[tokio::test]
async fn an_or_in_the_base_query_keeps_its_grouping() {
    let pool = seed().await;

    let mut writer = QueryWriter::<sqlx::Sqlite>::new(
        "SELECT id, name FROM users WHERE name = 'edsger' OR name = 'alan'",
    )
    .unwrap();
    writer.filter_by("tenant_id = ?").bind(1_i64);

    let users: Vec<User> = writer
        .build_as::<User>()
        .unwrap()
        .fetch_all(&pool)
        .await
        .unwrap();

    // Only alan: edsger is in tenant 2, and the tenant filter applies to both
    // sides of the OR rather than just the last one.
    assert_eq!(
        users,
        vec![User {
            id: 3,
            name: "alan".into()
        }]
    );
}

/// `build` rather than `build_as`, to cover the other constructor.
#[tokio::test]
async fn build_returns_a_runnable_query() {
    let pool = seed().await;

    let mut writer =
        QueryWriter::<sqlx::Sqlite>::new("SELECT count(*) AS n FROM users WHERE tenant_id = ?")
            .unwrap();
    writer.bind(1_i64);

    let row = writer.build().unwrap().fetch_one(&pool).await.unwrap();

    assert_eq!(row.get::<i64, _>("n"), 3);
}
