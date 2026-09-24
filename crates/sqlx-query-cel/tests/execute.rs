//! Filters run against real engines.
//!
//! The unit tests compare rendered SQL, which cannot tell whether a
//! `LIKE ... ESCAPE '!'` pattern matches what it should: that is each
//! engine's reading of the pattern, not the text's. SQLite runs in memory;
//! PostgreSQL runs when `SQLX_QUERY_POSTGRES_URL` names a reachable server,
//! as in `sqlx-query`'s own live-driver tests.

use std::collections::HashMap;

use sqlx::{PgPool, Row, SqlitePool};
use sqlx_query::{QueryComposer, QueryResolver};
use sqlx_query_cel::FilterClause;

/// Names chosen so that each pattern's wildcards, taken literally or not,
/// select different rows.
const NAMES: [&str; 5] = ["50%_off!", "50% off", "500_off", "Groceries", "groceries"];

fn columns() -> HashMap<&'static str, &'static str> {
    HashMap::from([("name", "name")])
}

/// The names `filter` selects from [`NAMES`], sorted.
async fn sqlite(filter: &str) -> Vec<String> {
    let pool = SqlitePool::connect("sqlite::memory:")
        .await
        .expect("connect");
    sqlx::query("CREATE TABLE items (name TEXT NOT NULL)")
        .execute(&pool)
        .await
        .expect("create");
    for name in NAMES {
        sqlx::query("INSERT INTO items VALUES (?)")
            .bind(name)
            .execute(&pool)
            .await
            .expect("insert");
    }

    let filter = FilterClause::parse(filter)
        .and_then(|filter| filter.resolve(&columns()))
        .expect("the filter resolves");
    let mut query = QueryComposer::<sqlx::Sqlite>::new(
        "SELECT name FROM items WHERE /* query.where AND */ TRUE ORDER BY name",
    );
    query.push_where(filter);

    query
        .build()
        .expect("composes")
        .fetch_all(&pool)
        .await
        .expect("runs")
        .iter()
        .map(|row| row.get::<String, _>("name"))
        .collect()
}

#[tokio::test]
async fn a_pattern_matches_its_wildcards_literally() {
    // `%` and `_` in the argument are the characters themselves, and so is
    // the escape character.
    assert_eq!(sqlite("name.startsWith('50%_')").await, ["50%_off!"]);
    assert_eq!(sqlite("name.endsWith('off!')").await, ["50%_off!"]);
    assert_eq!(sqlite("name.contains('0_o')").await, ["500_off"]);
}

#[tokio::test]
async fn a_pattern_matches_a_prefix_a_suffix_and_a_substring() {
    assert_eq!(sqlite("name.startsWith('50')").await.len(), 3);
    assert_eq!(sqlite("name.endsWith('ies')").await.len(), 2);
    assert_eq!(sqlite("name.contains('cer')").await.len(), 2);
}

/// `None` when there is no server to talk to -- see `sqlx-query`'s
/// `execute_postgres.rs` for why reachability, not the variable, decides.
async fn postgres() -> Option<PgPool> {
    let Ok(url) = std::env::var("SQLX_QUERY_POSTGRES_URL") else {
        eprintln!("skipped: SQLX_QUERY_POSTGRES_URL is unset");
        return None;
    };
    match PgPool::connect(&url).await {
        Ok(pool) => Some(pool),
        Err(error) => {
            eprintln!("skipped: no reachable postgres -- {error}");
            None
        }
    }
}

/// PostgreSQL's `LIKE` is case-sensitive and SQLite's is not for ASCII, so
/// the same pattern is checked on both rather than assumed.
#[tokio::test]
async fn a_pattern_reads_the_same_on_postgres() {
    let Some(pool) = postgres().await else {
        return;
    };

    let filter = FilterClause::parse("name.startsWith('50%_') || name.endsWith('ies')")
        .and_then(|filter| filter.resolve(&columns()))
        .expect("the filter resolves");
    let mut query = QueryComposer::<sqlx::Postgres>::new(
        "SELECT name FROM unnest($1::text[]) AS items(name) \
         WHERE /* query.where AND */ TRUE",
    );
    query.bind(NAMES.map(str::to_owned).to_vec());
    query.push_where(filter);

    let mut names: Vec<String> = query
        .build()
        .expect("composes")
        .fetch_all(&pool)
        .await
        .expect("runs")
        .iter()
        .map(|row| row.get::<String, _>("name"))
        .collect();
    // Sorted here rather than by `ORDER BY`, whose order is the database's
    // collation, not a byte order.
    names.sort();

    assert_eq!(names, ["50%_off!", "Groceries", "groceries"]);
}

/// A `timestamp("...")` literal binds as a timestamp, so it compares with a
/// `timestamptz` column -- where a string would not: PostgreSQL has no
/// `timestamptz > text`.
#[cfg(any(feature = "chrono", feature = "time"))]
#[tokio::test]
async fn a_timestamp_literal_compares_with_a_timestamp_column() {
    let Some(pool) = postgres().await else {
        return;
    };

    let filter = FilterClause::parse("created_at >= timestamp('2026-01-01T00:00:00Z')")
        .and_then(|filter| filter.resolve(&HashMap::from([("created_at", "created_at")])))
        .expect("the filter resolves");
    let mut query = QueryComposer::<sqlx::Postgres>::new(
        "SELECT name FROM (VALUES \
             ('before', '2025-12-31T23:59:59Z'::timestamptz), \
             ('on',     '2026-01-01T00:00:00Z'::timestamptz), \
             ('after',  '2026-01-02T00:00:00+02:00'::timestamptz) \
         ) AS items(name, created_at) \
         WHERE /* query.where AND */ TRUE",
    );
    query.push_where(filter);

    let mut names: Vec<String> = query
        .build()
        .expect("composes")
        .fetch_all(&pool)
        .await
        .expect("runs")
        .iter()
        .map(|row| row.get::<String, _>("name"))
        .collect();
    names.sort();

    assert_eq!(names, ["after", "on"]);
}
