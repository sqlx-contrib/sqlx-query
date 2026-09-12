//! Pagination against a real database.
//!
//! Everything else in this crate asserts on generated SQL, which proves the
//! shape and not the behaviour. The property that matters cannot be checked
//! that way: paging through a table has to visit every row exactly once,
//! including rows that tie on the sort column.
#![cfg(all(feature = "sqlite", feature = "cel"))]

use sqlx::{AssertSqlSafe, Row as _, Sqlite, SqlitePool};
use sqlx_query::{Column, ColumnType, Cursor, Filter, QueryMapping, QueryTemplate, Sort};

/// Note `LIMIT 3` is a literal, not a bind. SQLite numbers placeholders by
/// their position in the text, so a `?` after a slot would be shifted by
/// whatever the slot splices in front of it -- which the crate now refuses
/// rather than binding wrongly.
const PAGE: &str = "SELECT id, title, read_count FROM volumes \
                    WHERE tenant_id = ? \
                    /* AND query.filter */ \
                    /* ORDER BY query.order */ \
                    LIMIT 3";

fn mapping() -> QueryMapping {
    QueryMapping::new()
        .key("id", ColumnType::Int)
        .column("title", ColumnType::Text)
        .add("readCount", Column::new("read_count", ColumnType::Int))
}

async fn seed() -> SqlitePool {
    let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();

    sqlx::raw_sql(
        "CREATE TABLE volumes (
             id INTEGER PRIMARY KEY,
             tenant_id INTEGER NOT NULL,
             title TEXT NOT NULL,
             read_count INTEGER NOT NULL
         )",
    )
    .execute(&pool)
    .await
    .unwrap();

    // `read_count` ties deliberately: without a tiebreaker these rows have no
    // defined order, which is the failure keyset pagination has to survive.
    for (id, title, reads) in [
        (1, "Alpha", 100),
        (2, "Bravo", 100),
        (3, "Charlie", 100),
        (4, "Delta", 90),
        (5, "Echo", 90),
        (6, "Foxtrot", 80),
        (7, "Golf", 80),
        (8, "Hotel", 70),
        (9, "India", 60),
        // A different tenant, which must never appear.
        (10, "Juliet", 999),
    ] {
        sqlx::query("INSERT INTO volumes (id, tenant_id, title, read_count) VALUES (?, ?, ?, ?)")
            .bind(id)
            .bind(if id == 10 { 2_i64 } else { 1_i64 })
            .bind(title)
            .bind(reads)
            .execute(&pool)
            .await
            .unwrap();
    }

    pool
}

/// Page through with a small page size and check the concatenation against the
/// same query run in one go.
async fn page_through(pool: &SqlitePool, order_by: &str, filter: &str) -> Vec<i64> {
    let template = QueryTemplate::<Sqlite>::parse(PAGE).unwrap();
    let mapping = mapping();

    let filter = Filter::parse(filter).unwrap();
    let sort = Sort::parse(order_by).unwrap().asc("id");

    let mut cursor = Cursor::parse("").unwrap();
    let mut seen = Vec::new();

    loop {
        cursor.validate(&sort).unwrap();

        let rows = template
            .builder()
            .bind(1_i64)
            .fill("filter", &filter.to_fragment(&mapping).unwrap())
            .fill("filter", &cursor.to_fragment(&mapping).unwrap())
            .fill("order", &sort.to_fragment(&mapping).unwrap())
            .build()
            .unwrap()
            .fetch_all(pool)
            .await
            .unwrap();

        let Some(last) = rows.last() else { break };

        seen.extend(rows.iter().map(|row| row.get::<i64, _>("id")));

        // The token for the next page, read out of the row by the mapping's
        // own field-to-column mapping.
        cursor = Cursor::new(&sort).after(last, &mapping).unwrap();
    }

    seen
}

async fn all_at_once(pool: &SqlitePool, order_sql: &str, where_sql: &str) -> Vec<i64> {
    sqlx::query(AssertSqlSafe(format!(
        "SELECT id FROM volumes WHERE tenant_id = 1 {where_sql} ORDER BY {order_sql}"
    )))
    .fetch_all(pool)
    .await
    .unwrap()
    .iter()
    .map(|row| row.get::<i64, _>("id"))
    .collect()
}

#[tokio::test]
async fn paging_visits_every_row_exactly_once() {
    let pool = seed().await;

    let paged = page_through(&pool, "readCount desc", "").await;
    let whole = all_at_once(&pool, r#""read_count" DESC, "id" ASC"#, "").await;

    assert_eq!(paged, whole);
    assert_eq!(paged.len(), 9, "the other tenant's row leaked in");
}

/// Mixed directions are the case a row-value comparison cannot express, so the
/// seek condition expands into an OR-chain. This is what proves the chain.
#[tokio::test]
async fn paging_survives_mixed_directions() {
    let pool = seed().await;

    let paged = page_through(&pool, "readCount asc", "").await;
    let whole = all_at_once(&pool, r#""read_count" ASC, "id" ASC"#, "").await;

    assert_eq!(paged, whole);
}

#[tokio::test]
async fn paging_holds_with_a_filter_applied() {
    let pool = seed().await;

    let paged = page_through(&pool, "readCount desc", "readCount >= 80").await;
    let whole = all_at_once(
        &pool,
        r#""read_count" DESC, "id" ASC"#,
        "AND read_count >= 80",
    )
    .await;

    assert_eq!(paged, whole);
    assert_eq!(paged.len(), 7);
}

/// A LIKE needle containing a wildcard must match literally, not everything.
#[tokio::test]
async fn like_wildcards_in_a_filter_are_escaped() {
    let pool = seed().await;

    sqlx::query(
        "INSERT INTO volumes (id, tenant_id, title, read_count) VALUES (11, 1, '100%', 50)",
    )
    .execute(&pool)
    .await
    .unwrap();

    let paged = page_through(&pool, "readCount desc", "title.contains('100%')").await;

    assert_eq!(paged, [11]);
}

// ---------------------------------------------------------------------------
// Joins
// ---------------------------------------------------------------------------

/// A joined column needs `"a"."name"` in the `WHERE`, because SQL evaluates it
/// before `SELECT` and the alias is not in scope there — while the cursor reads
/// `author_name`, because that is what the returned row calls it. One field,
/// two names, used in different places.
const JOINED: &str = "SELECT v.id, v.title, a.name AS author_name \
                      FROM volumes v JOIN authors a ON a.id = v.author_id \
                      WHERE v.tenant_id = ? \
                      /* AND query.filter */ \
                      /* ORDER BY query.order */ \
                      LIMIT 2";

fn joined_mapping() -> QueryMapping {
    QueryMapping::new()
        .add("id", Column::key("id", ColumnType::Int).with_qualifier("v"))
        .add(
            "title",
            Column::new("title", ColumnType::Text).with_qualifier("v"),
        )
        .add(
            "authorName",
            Column::new("name", ColumnType::Text)
                .with_qualifier("a")
                .with_alias("author_name"),
        )
}

async fn seed_authors(pool: &SqlitePool) {
    sqlx::raw_sql(
        "CREATE TABLE authors (id INTEGER PRIMARY KEY, name TEXT NOT NULL);
         ALTER TABLE volumes ADD COLUMN author_id INTEGER NOT NULL DEFAULT 1;
         INSERT INTO authors (id, name) VALUES (1, 'Herbert'), (2, 'Le Guin');
         UPDATE volumes SET author_id = 2 WHERE id % 2 = 0;",
    )
    .execute(pool)
    .await
    .unwrap();
}

#[tokio::test]
async fn a_join_pages_by_a_qualified_and_aliased_column() {
    let pool = seed().await;
    seed_authors(&pool).await;

    let template = QueryTemplate::<Sqlite>::parse(JOINED).unwrap();
    let mapping = joined_mapping();
    let sort = Sort::parse("authorName desc").unwrap().asc("id");

    let mut cursor = Cursor::parse("").unwrap();
    let mut seen = Vec::new();

    loop {
        cursor.validate(&sort).unwrap();

        let rows = template
            .builder()
            .bind(1_i64)
            .fill("filter", &cursor.to_fragment(&mapping).unwrap())
            .fill("order", &sort.to_fragment(&mapping).unwrap())
            .build()
            .unwrap()
            .fetch_all(&pool)
            .await
            .unwrap();

        let Some(last) = rows.last() else { break };

        seen.extend(rows.iter().map(|row| row.get::<i64, _>("id")));

        // Reads `author_name` from the row, not `a.name`.
        cursor = Cursor::new(&sort).after(last, &mapping).unwrap();
    }

    let whole: Vec<i64> = sqlx::query(
        "SELECT v.id FROM volumes v JOIN authors a ON a.id = v.author_id \
         WHERE v.tenant_id = 1 ORDER BY a.name DESC, v.id ASC",
    )
    .fetch_all(&pool)
    .await
    .unwrap()
    .iter()
    .map(|row| row.get::<i64, _>("id"))
    .collect();

    assert_eq!(seen, whole);
    assert_eq!(seen.len(), 9);
}
