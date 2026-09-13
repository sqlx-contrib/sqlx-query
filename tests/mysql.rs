//! Pagination against a real MySQL server.
//!
//! The mirror of `tests/postgres.rs`, and the point is where the two differ.
//! MySQL's `?` names the *N*th placeholder *in the text*, so splicing shifts
//! every placeholder after the splice point onto the wrong value. This crate
//! refuses the arrangements where that would happen -- a bind after a fill,
//! slots filled out of order, a `?` in the skeleton after a slot -- and those
//! refusals are unit-tested. What is left, and what only a server can confirm,
//! is that the arrangement it *does* allow lands every value on the placeholder
//! it was bound for.
//!
//! Hence `LIMIT 3` as a literal here where PostgreSQL has `LIMIT $2`: the same
//! skeleton on this driver is an error, by design.
//!
//! Skips when `SQLX_QUERY_MYSQL_URL` is unset, so `cargo test` is green outside
//! the dev shell -- and covers less. `make databases` says which way it ran.
#![cfg(all(feature = "mysql", feature = "cel"))]

use chrono::{DateTime, TimeZone as _, Utc};
use sqlx::{AssertSqlSafe, Connection as _, MySqlConnection, Row as _};
use sqlx_query::{Column, ColumnType, Cursor, Filter, QueryMapping, QueryTemplate, Sort};

const ENV_URL: &str = "SQLX_QUERY_MYSQL_URL";

/// Every placeholder the skeleton owns sits *before* every slot, which is the
/// only ordering a positional driver can splice into. The page size is a
/// literal for the same reason: a `?` after a slot is refused.
const PAGE: &str = "SELECT id, title, read_count, price, archived, published_at \
                    FROM volumes \
                    WHERE tenant_id = ? \
                    /* AND query.filter */ \
                    /* ORDER BY query.order */ \
                    LIMIT 3";

fn mapping() -> QueryMapping {
    QueryMapping::new()
        .key("id", ColumnType::Int)
        .column("title", ColumnType::Text)
        .add("readCount", Column::new("read_count", ColumnType::Int))
        .add("price", Column::new("price", ColumnType::Float))
        .add("archived", Column::new("archived", ColumnType::Bool))
        .add(
            "publishedAt",
            Column::new("published_at", ColumnType::Timestamp),
        )
}

fn at(day: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2024, 1, day, 12, 0, 0).unwrap()
}

/// Connect and seed, or `None` when no server is configured.
///
/// One connection rather than a pool, and the table is `TEMPORARY`: temporary
/// tables belong to the connection that made them, so every test gets its own
/// `volumes` and they run in parallel without a schema, a migration tool or any
/// cleanup. MySQL also commits implicitly around DDL, so a transaction could
/// not have given the same isolation.
async fn connect() -> Option<MySqlConnection> {
    let url = std::env::var(ENV_URL)
        .ok()
        .filter(|value| !value.trim().is_empty())?;

    let mut connection = MySqlConnection::connect(&url)
        .await
        .expect("connect to the test database");

    sqlx::raw_sql(
        "CREATE TEMPORARY TABLE volumes (
             id           BIGINT PRIMARY KEY,
             tenant_id    BIGINT NOT NULL,
             title        VARCHAR(255) NOT NULL,
             read_count   BIGINT NOT NULL,
             price        DOUBLE NOT NULL,
             archived     BOOLEAN NOT NULL,
             published_at TIMESTAMP NOT NULL
         )",
    )
    .execute(&mut connection)
    .await
    .expect("create the temporary table");

    // `read_count` and `published_at` tie deliberately: rows that tie on the
    // sort column have no defined order between them, which is the failure
    // keyset pagination has to survive.
    for (id, title, reads, price, archived, day) in [
        (1_i64, "Alpha", 100_i64, 9.99_f64, false, 1_u32),
        (2, "Bravo", 100, 19.99, false, 1),
        (3, "Charlie", 100, 4.99, true, 2),
        (4, "Delta", 90, 14.99, false, 2),
        (5, "Echo", 90, 24.99, false, 3),
        (6, "Foxtrot", 80, 7.99, true, 3),
        (7, "Golf", 80, 11.99, false, 4),
        (8, "Hotel", 70, 3.99, false, 4),
        (9, "India", 60, 29.99, false, 5),
        // A different tenant, which must never appear.
        (10, "Juliet", 999, 0.99, false, 5),
    ] {
        sqlx::query(
            "INSERT INTO volumes (id, tenant_id, title, read_count, price, archived, published_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(id)
        .bind(if id == 10 { 2_i64 } else { 1_i64 })
        .bind(title)
        .bind(reads)
        .bind(price)
        .bind(archived)
        .bind(at(day))
        .execute(&mut connection)
        .await
        .expect("seed a row");
    }

    Some(connection)
}

/// Define a test that skips when no server is configured.
///
/// The skip notice only surfaces under `cargo test -- --nocapture`; by default
/// a skipped test is indistinguishable from a passing one, which is what `make
/// databases` is for.
macro_rules! db_test {
    ($(#[$attribute:meta])* async fn $name:ident($connection:ident) $body:block) => {
        $(#[$attribute])*
        #[tokio::test]
        async fn $name() {
            let Some(mut $connection) = connect().await else {
                eprintln!("skipping {}: {} is not set", stringify!($name), ENV_URL);
                return;
            };

            $body
        }
    };
}

/// Page through the listing, returning the ids in the order they were visited.
async fn page_through(connection: &mut MySqlConnection, order_by: &str, filter: &str) -> Vec<i64> {
    let template = QueryTemplate::parse(PAGE).unwrap();
    let mapping = mapping();

    let filter = Filter::parse(filter).unwrap().resolve(&mapping).unwrap();
    let sort = Sort::parse(order_by)
        .unwrap()
        .asc("id")
        .resolve(&mapping)
        .unwrap();

    let mut cursor = Cursor::parse("").unwrap().resolve(&mapping).unwrap();
    let mut seen = Vec::new();

    loop {
        let rows = template
            .builder()
            .bind(1_i64) // the tenant, and the only placeholder before a slot
            .filter(&filter)
            .seek(&cursor)
            .order(&sort)
            .build()
            .unwrap()
            .fetch_all(&mut *connection)
            .await
            .unwrap();

        let Some(last) = rows.last() else { break };

        seen.extend(rows.iter().map(|row| row.get::<i64, _>("id")));

        // A cursor that does not advance pages forever. Nine rows can never
        // need this many.
        assert!(seen.len() < 100, "the cursor is not advancing");

        cursor = Cursor::new(&sort).after(last).unwrap();
    }

    seen
}

/// The same listing, unpaged, as the answer to compare against.
async fn all_at_once(
    connection: &mut MySqlConnection,
    order_sql: &str,
    where_sql: &str,
) -> Vec<i64> {
    sqlx::query(AssertSqlSafe(format!(
        "SELECT id FROM volumes WHERE tenant_id = 1 {where_sql} ORDER BY {order_sql}"
    )))
    .fetch_all(connection)
    .await
    .unwrap()
    .iter()
    .map(|row| row.get::<i64, _>("id"))
    .collect()
}

db_test! {
    async fn paging_visits_every_row_exactly_once(connection) {
        let paged = page_through(&mut connection, "readCount desc", "").await;
        let whole = all_at_once(&mut connection, "`read_count` DESC, `id` ASC", "").await;

        assert_eq!(paged, whole);
        assert_eq!(paged.len(), 9, "the other tenant's row leaked in");
    }
}

db_test! {
    /// Mixed directions are the case a row-value comparison cannot express, so
    /// the seek condition expands into an OR-chain. This is what proves the
    /// chain against a server rather than against a string.
    async fn paging_survives_mixed_directions(connection) {
        let paged = page_through(&mut connection, "readCount asc", "").await;
        let whole = all_at_once(&mut connection, "`read_count` ASC, `id` ASC", "").await;

        assert_eq!(paged, whole);
    }
}

db_test! {
    async fn paging_holds_with_a_filter_applied(connection) {
        let paged = page_through(&mut connection, "readCount desc", "readCount >= 80").await;
        let whole = all_at_once(
            &mut connection,
            "`read_count` DESC, `id` ASC",
            "AND read_count >= 80",
        )
        .await;

        assert_eq!(paged, whole);
        assert_eq!(paged.len(), 7);
    }
}

db_test! {
    /// The claim in one test, stated directly: a value bound before the splice
    /// and seven bound by it, and every one of them lands on the placeholder it
    /// was bound for. The tenant is the one that would go wrong first -- shift
    /// it by one and the query reads a filter value as the tenant id, which
    /// silently returns another tenant's rows or none at all.
    async fn every_value_lands_on_its_own_placeholder(connection) {
        let mapping = mapping();
        let template = QueryTemplate::parse(PAGE).unwrap();

        let filter = Filter::parse("readCount >= 80 && price > 5.0 && archived == false")
            .unwrap()
            .resolve(&mapping)
            .unwrap();
        let sort = Sort::parse("readCount desc").unwrap().asc("id").resolve(&mapping).unwrap();

        // A seek from the first row of that listing, so the query carries a
        // filter's values and a cursor's together in one slot.
        let cursor = Cursor::new(&sort)
            .resolve(&mapping)
            .unwrap();
        let first = template
            .builder()
            .bind(1_i64)
            .filter(&filter)
            .seek(&cursor)
            .order(&sort)
            .build()
            .unwrap()
            .fetch_all(&mut connection)
            .await
            .unwrap();

        assert_eq!(first.iter().map(|row| row.get::<i64, _>("id")).collect::<Vec<_>>(), [1, 2, 4]);

        let cursor = Cursor::new(&sort).after(first.last().unwrap()).unwrap();
        let query = template
            .builder()
            .bind(1_i64)
            .filter(&filter)
            .seek(&cursor)
            .order(&sort);

        // One for the tenant, three for the filter, three for the seek chain.
        assert_eq!(query.sql().matches('?').count(), 7, "{}", query.sql());

        let second = query.build().unwrap().fetch_all(&mut connection).await.unwrap();

        // The rest of the listing: [1, 2, 4, 5, 7] less the page already seen.
        assert_eq!(
            second.iter().map(|row| row.get::<i64, _>("id")).collect::<Vec<_>>(),
            [5, 7],
        );
    }
}

db_test! {
    /// A LIKE needle containing a wildcard must match literally, not everything.
    async fn like_wildcards_in_a_filter_are_escaped(connection) {
        sqlx::query(
            "INSERT INTO volumes (id, tenant_id, title, read_count, price, archived, published_at) \
             VALUES (11, 1, '100%', 50, 1.99, false, ?)",
        )
        .bind(at(6))
        .execute(&mut connection)
        .await
        .unwrap();

        let paged = page_through(&mut connection, "readCount desc", "title.contains('100%')").await;

        assert_eq!(paged, [11]);
    }
}

db_test! {
    /// A timestamp key goes into the token as seconds and nanoseconds and comes
    /// back out as a bind against a `TIMESTAMP` column. Nothing else exercises
    /// that round trip through a driver with its own wire format for the type.
    async fn paging_by_a_timestamp_round_trips_through_the_token(connection) {
        let paged = page_through(&mut connection, "publishedAt desc", "").await;
        let whole = all_at_once(&mut connection, "`published_at` DESC, `id` ASC", "").await;

        assert_eq!(paged, whole);
        assert_eq!(paged.len(), 9);
    }
}

db_test! {
    /// Backticks, and the reason the quote character is per-driver: the same
    /// identifier quoted PostgreSQL's way is a string literal here, and
    /// `WHERE "read_count" > 80` would compare a constant against a number
    /// rather than reading the column.
    async fn a_filter_binds_floats_and_booleans(connection) {
        let paged = page_through(
            &mut connection,
            "price asc",
            "price > 9.99 && archived == false",
        )
        .await;
        let whole = all_at_once(
            &mut connection,
            "`price` ASC, `id` ASC",
            "AND price > 9.99 AND archived = false",
        )
        .await;

        assert_eq!(paged, whole);
        assert_eq!(paged, [7, 4, 2, 5, 9]);
    }
}
