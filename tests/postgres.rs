//! Pagination against a real PostgreSQL server.
//!
//! What can only be checked here: `$N` names the *N*th bound value, so a
//! `LIMIT $2` the skeleton already had keeps its meaning when a filter and a
//! seek condition splice `$3` onwards in ahead of it. Every other test in this
//! crate asserts on generated SQL, which shows that the text says `LIMIT $2`
//! and not that the server reads it as the page size.
//!
//! Skips when `SQLX_QUERY_POSTGRES_URL` is unset, so `cargo test` is green
//! outside the dev shell -- and covers less. `make databases` says which way it
//! ran.
#![cfg(all(feature = "postgres", feature = "cel"))]

use chrono::{DateTime, TimeZone as _, Utc};
use sqlx::{AssertSqlSafe, Connection as _, PgConnection, Row as _};
use sqlx_query::{Column, ColumnType, Cursor, Filter, QueryMapping, QueryTemplate, Sort};

const ENV_URL: &str = "SQLX_QUERY_POSTGRES_URL";

/// Small enough that nine rows take several pages, so a cursor is actually
/// exercised rather than one page covering everything.
const PAGE_SIZE: i64 = 3;

/// `LIMIT $2` is bound *before* anything is spliced, and rendered after.
///
/// That is the arrangement this whole file exists to check. On a positional
/// driver it is refused outright; here it has to work, because `$2` names the
/// second value bound and not the second placeholder in the text.
const PAGE: &str = "SELECT id, title, read_count, price, archived, published_at \
                    FROM volumes \
                    WHERE tenant_id = $1 \
                    /* AND query.filter */ \
                    /* ORDER BY query.order */ \
                    LIMIT $2";

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
/// tables are scoped to the session that made them, so every test gets its own
/// `volumes` and they can run in parallel without a schema, a migration tool or
/// any cleanup. A pool would undo that -- it may hand back a different
/// connection, which cannot see the table.
async fn connect() -> Option<PgConnection> {
    let url = std::env::var(ENV_URL)
        .ok()
        .filter(|u| !u.trim().is_empty())?;

    let mut connection = PgConnection::connect(&url)
        .await
        .expect("connect to the test database");

    sqlx::raw_sql(
        "CREATE TEMPORARY TABLE volumes (
             id           BIGINT PRIMARY KEY,
             tenant_id    BIGINT NOT NULL,
             title        TEXT NOT NULL,
             read_count   BIGINT NOT NULL,
             price        DOUBLE PRECISION NOT NULL,
             archived     BOOLEAN NOT NULL,
             published_at TIMESTAMPTZ NOT NULL
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
             VALUES ($1, $2, $3, $4, $5, $6, $7)",
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
async fn page_through(connection: &mut PgConnection, order_by: &str, filter: &str) -> Vec<i64> {
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
            .bind(1_i64) // $1, the tenant
            .bind(PAGE_SIZE) // $2, the limit -- rendered after the splice
            .filter(&filter)
            .seek(&cursor)
            .order(&sort)
            .build()
            .unwrap()
            .fetch_all(&mut *connection)
            .await
            .unwrap();

        assert!(
            i64::try_from(rows.len()).unwrap() <= PAGE_SIZE,
            "a page of {} rows: LIMIT $2 did not survive the splice",
            rows.len()
        );

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
async fn all_at_once(connection: &mut PgConnection, order_sql: &str, where_sql: &str) -> Vec<i64> {
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
        let whole = all_at_once(&mut connection, r#""read_count" DESC, "id" ASC"#, "").await;

        assert_eq!(paged, whole);
        assert_eq!(paged.len(), 9, "the other tenant's row leaked in");
    }
}

db_test! {
    /// Mixed directions are the case a row-value comparison cannot express, so the
    /// seek condition expands into an OR-chain. This is what proves the chain
    /// against a server rather than against a string.
    async fn paging_survives_mixed_directions(connection) {
        let paged = page_through(&mut connection, "readCount asc", "").await;
        let whole = all_at_once(&mut connection, r#""read_count" ASC, "id" ASC"#, "").await;

        assert_eq!(paged, whole);
    }
}

db_test! {
    async fn paging_holds_with_a_filter_applied(connection) {
        let paged = page_through(&mut connection, "readCount desc", "readCount >= 80").await;
        let whole = all_at_once(
            &mut connection,
            r#""read_count" DESC, "id" ASC"#,
            "AND read_count >= 80",
        )
        .await;

        assert_eq!(paged, whole);
        assert_eq!(paged.len(), 7);
    }
}

db_test! {
    /// The claim in one test, stated directly: bind the page size second, splice
    /// four placeholders in ahead of it, and the server still reads `$2` as the
    /// page size. Under positional numbering `$2` would be the filter's first
    /// value, and PostgreSQL would refuse a `bigint` LIMIT of `100` -- or worse,
    /// accept one.
    async fn a_bind_before_a_splice_keeps_its_number(connection) {
        let mapping = mapping();
        let template = QueryTemplate::parse(PAGE).unwrap();

        let filter = Filter::parse("readCount >= 80 && archived == false")
            .unwrap()
            .resolve(&mapping)
            .unwrap();
        let sort = Sort::parse("readCount desc").unwrap().asc("id").resolve(&mapping).unwrap();

        let query = template
            .builder()
            .bind(1_i64)
            .bind(2_i64)      // a page size of two, as $2
            .filter(&filter)
            .order(&sort);

        assert!(query.sql().ends_with("LIMIT $2"), "{}", query.sql());
        assert!(query.sql().contains("$3"), "nothing was spliced: {}", query.sql());

        let rows = query.build().unwrap().fetch_all(&mut connection).await.unwrap();

        assert_eq!(rows.len(), 2, "$2 was not read as the page size");
        assert_eq!(
            rows.iter().map(|row| row.get::<i64, _>("id")).collect::<Vec<_>>(),
            [1, 2],
        );
    }
}

db_test! {
    /// A LIKE needle containing a wildcard must match literally, not everything.
    async fn like_wildcards_in_a_filter_are_escaped(connection) {
        sqlx::query(
            "INSERT INTO volumes (id, tenant_id, title, read_count, price, archived, published_at) \
             VALUES (11, 1, '100%', 50, 1.99, false, $1)",
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
    /// back out as a bind against `timestamptz`. Nothing else exercises that round
    /// trip through a driver that has its own wire format for the type.
    async fn paging_by_a_timestamp_round_trips_through_the_token(connection) {
        let paged = page_through(&mut connection, "publishedAt desc", "").await;
        let whole = all_at_once(&mut connection, r#""published_at" DESC, "id" ASC"#, "").await;

        assert_eq!(paged, whole);
        assert_eq!(paged.len(), 9);
    }
}

db_test! {
    /// Floats and booleans bind through the same path as everything else, and a
    /// CEL `false` has to reach the server as a boolean rather than as the string
    /// `false` -- which PostgreSQL, unlike the other two, will reject outright.
    async fn a_filter_binds_floats_and_booleans(connection) {
        let paged = page_through(
            &mut connection,
            "price asc",
            "price > 9.99 && archived == false",
        )
        .await;
        let whole = all_at_once(
            &mut connection,
            r#""price" ASC, "id" ASC"#,
            "AND price > 9.99 AND archived = false",
        )
        .await;

        assert_eq!(paged, whole);
        assert_eq!(paged, [7, 4, 2, 5, 9]);
    }
}
