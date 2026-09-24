//! The PostgreSQL half of the live-driver tests.
//!
//! Skipped when `SQLX_QUERY_POSTGRES_URL` is unset, so `make test` stays
//! offline. The Dev Container sets it; from a host, start the compose file
//! and point it at the mapped port.
//!
//! What these check that SQLite can't: `$N` binds by *number*, so a
//! placeholder referenced twice consumes one value rather than two — the
//! exact opposite of the `?` path, and the reason the composer renders the
//! two dialects differently.

#![cfg(feature = "postgres")]

use sqlx::{PgPool, Row};
use sqlx_query::{Cursor, OrderByClause, Pager, QueryComposer, WhereClause};

/// `None` when there is no server to talk to, which is not the same as
/// the variable being unset: the Nix dev shell exports the Dev Container's
/// `containerEnv` verbatim when the compose stack is down, so the URL can
/// be present and name a host that only resolves inside that network.
/// Reachability is the only thing worth testing here.
async fn pool() -> Option<PgPool> {
    let Ok(url) = std::env::var("SQLX_QUERY_POSTGRES_URL") else {
        eprintln!("skipped: SQLX_QUERY_POSTGRES_URL is unset");
        return None;
    };
    match PgPool::connect(&url).await {
        Ok(pool) => Some(pool),
        Err(error) => {
            // Deliberately without the URL, which carries credentials.
            eprintln!("skipped: no reachable postgres -- {error}");
            None
        }
    }
}

/// Each test owns a table, because they run in parallel.
async fn table(pool: &PgPool, name: &str, columns: &str) {
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
            // `pool` already said why.
            None => return,
        }
    };
}

/// The `$N` counterpart of the SQLite ordering test: numbering makes the
/// slot's position irrelevant, so the values stay in bind-declaration
/// order and still land on the right columns.
#[tokio::test]
async fn a_slot_ahead_of_the_base_placeholders_still_binds_by_number() {
    let pool = pool_or_skip!();
    table(
        &pool,
        "pg_order_test",
        "id bigint primary key, tenant_id text not null, rank bigint not null",
    )
    .await;

    for (id, tenant, rank) in [(1i64, "acme", 10i64), (2, "acme", 30), (3, "other", 40)] {
        sqlx::query("INSERT INTO pg_order_test VALUES ($1, $2, $3)")
            .bind(id)
            .bind(tenant)
            .bind(rank)
            .execute(&pool)
            .await
            .expect("insert");
    }

    let sql = "SELECT id FROM pg_order_test \
               WHERE /* query.where AND */ tenant_id = $1 \
               ORDER BY /* query.order_by , */ id LIMIT $2";
    let mut query = QueryComposer::<sqlx::Postgres>::new(sql);
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
    assert_eq!(ids, vec![2]);
}

/// A cursor's tuple comparison references each boundary value twice. On
/// `$N` that is one bound value serving two references — the opposite of
/// the `?` rendering, and the case that would break if the composer
/// duplicated values here too.
#[tokio::test]
async fn a_repeated_placeholder_consumes_one_value() {
    let pool = pool_or_skip!();
    table(
        &pool,
        "pg_cursor_test",
        "id bigint primary key, rank bigint not null",
    )
    .await;

    for (id, rank) in [(1i64, 10i64), (2, 20), (3, 30)] {
        sqlx::query("INSERT INTO pg_cursor_test VALUES ($1, $2)")
            .bind(id)
            .bind(rank)
            .execute(&pool)
            .await
            .expect("insert");
    }

    let order_by = OrderByClause::default().desc("rank").asc("id");
    let sql = "SELECT id, rank FROM pg_cursor_test \
               WHERE /* query.where AND */ TRUE ORDER BY /* query.order_by , */ id";

    let mut first = QueryComposer::<sqlx::Postgres>::new(sql);
    first.push_order_by(order_by.clone());
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
        vec![3, 2, 1]
    );

    let cursor = Cursor::new(order_by)
        .after_row(&rows[0])
        .expect("cursor from the first row");

    let mut second = QueryComposer::<sqlx::Postgres>::new(sql);
    second.with_cursor(cursor);
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
        vec![2, 1],
        "everything after rank 30"
    );
}

/// `bind` erases the type, which is the only way to supply a parameter
/// `Value` has no variant for. PostgreSQL types every parameter on the
/// wire, so this also exercises `Encode::produces` for the erased path.
#[tokio::test]
async fn an_erased_uuid_binds_against_a_uuid_column() {
    let pool = pool_or_skip!();
    table(&pool, "pg_uuid_bind_test", "id uuid primary key").await;

    let id = "11111111-1111-1111-1111-111111111111";
    sqlx::query("INSERT INTO pg_uuid_bind_test VALUES ($1::uuid)")
        .bind(id)
        .execute(&pool)
        .await
        .expect("insert");

    // Bound as text with a cast, which is the workaround `Value`'s closed
    // set forces today. Without the cast PostgreSQL refuses `uuid = text`.
    let sql = "SELECT id::text AS id FROM pg_uuid_bind_test \
               WHERE /* query.where AND */ id = $1::uuid";
    let mut query = QueryComposer::<sqlx::Postgres>::new(sql);
    query.bind_value(id);

    let rows = query
        .build()
        .expect("composes")
        .fetch_all(&pool)
        .await
        .expect("runs");

    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].get::<String, _>("id"), id);
}

/// Without the `uuid` feature a cursor cannot page on a `uuid` key.
///
/// `RowExtension::get_value` tries `i64, i32, i16, f64, f32, bool,
/// DateTime<Utc>, String, Vec<u8>` in turn, and a PostgreSQL `uuid`
/// matches none of them — `String: Type<Postgres>` covers text and
/// varchar, not uuid. Decoding it as text wouldn't help on its own: the
/// value has to be *bound back* on the next request, and `uuid = text` has
/// no operator. The `uuid` feature is what closes it; see
/// [`a_cursor_pages_on_a_uuid_key`].
#[cfg(not(feature = "uuid"))]
#[tokio::test]
async fn a_cursor_cannot_page_on_a_uuid_key_without_the_feature() {
    let pool = pool_or_skip!();
    table(&pool, "pg_uuid_cursor_test", "id uuid primary key").await;

    sqlx::query(
        "INSERT INTO pg_uuid_cursor_test VALUES ('22222222-2222-2222-2222-222222222222'::uuid)",
    )
    .execute(&pool)
    .await
    .expect("insert");

    let rows = sqlx::query("SELECT id FROM pg_uuid_cursor_test")
        .fetch_all(&pool)
        .await
        .expect("select");

    let result = Cursor::new(OrderByClause::default().asc("id")).after_row(&rows[0]);

    assert!(
        matches!(result, Err(sqlx_query::CursorError::RowValueUndecodable(ref column)) if column == "id"),
        "expected the uuid gap without the feature, got {result:?}"
    );
}

/// A `Value::Uuid` binds as a `uuid`, so it compares with a `uuid` column
/// without the `::uuid` cast [`an_erased_uuid_binds_against_a_uuid_column`]
/// needs for text.
#[cfg(feature = "uuid")]
#[tokio::test]
async fn a_uuid_value_binds_against_a_uuid_column() {
    let pool = pool_or_skip!();
    table(&pool, "pg_uuid_value_test", "id uuid primary key").await;

    let id = uuid::Uuid::from_u128(0x3333_3333_3333_3333_3333_3333_3333_3333);
    sqlx::query("INSERT INTO pg_uuid_value_test VALUES ($1)")
        .bind(id)
        .execute(&pool)
        .await
        .expect("insert");

    let sql = "SELECT id FROM pg_uuid_value_test WHERE /* query.where AND */ TRUE";
    let mut query = QueryComposer::<sqlx::Postgres>::new(sql);
    query.push_where(WhereClause::new("id = $1").bind_value(id));

    let rows = query
        .build()
        .expect("composes")
        .fetch_all(&pool)
        .await
        .expect("runs");

    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].get::<uuid::Uuid, _>("id"), id);
}

/// With the `uuid` feature a cursor reads a `uuid` key back as a
/// `Value::Uuid` and binds it against the column on the next page — keyset
/// pagination on the commonest primary key there is.
#[cfg(feature = "uuid")]
#[tokio::test]
async fn a_cursor_pages_on_a_uuid_key() {
    let pool = pool_or_skip!();
    table(&pool, "pg_uuid_cursor_test", "id uuid primary key").await;

    let ids: Vec<uuid::Uuid> = (1..=3u128)
        .map(|n| uuid::Uuid::from_u128(n * 0x1111_1111_1111_1111_1111_1111_1111_1111))
        .collect();
    for id in &ids {
        sqlx::query("INSERT INTO pg_uuid_cursor_test VALUES ($1)")
            .bind(id)
            .execute(&pool)
            .await
            .expect("insert");
    }

    let order_by = OrderByClause::default().asc("id");
    let sql = "SELECT id FROM pg_uuid_cursor_test \
               WHERE /* query.where AND */ TRUE ORDER BY /* query.order_by */ LIMIT $1";

    let mut first = QueryComposer::<sqlx::Postgres>::new(sql);
    first.bind_value(1i64).push_order_by(order_by.clone());
    let rows = first
        .build()
        .expect("composes")
        .fetch_all(&pool)
        .await
        .expect("runs");
    assert_eq!(rows[0].get::<uuid::Uuid, _>("id"), ids[0]);

    // Through a token, as it would travel between requests.
    let token = Cursor::new(order_by)
        .after_row(&rows[0])
        .expect("a uuid key decodes")
        .encode();

    let mut second = QueryComposer::<sqlx::Postgres>::new(sql);
    second
        .bind_value(10i64)
        .with_cursor(Cursor::parse(&token).expect("the token parses"));
    let rows = second
        .build()
        .expect("composes")
        .fetch_all(&pool)
        .await
        .expect("runs");

    let rest: Vec<uuid::Uuid> = rows.iter().map(|row| row.get("id")).collect();
    assert_eq!(rest, ids[1..]);
}

/// The `$N` counterpart of SQLite's pager test: the pager's limit binds by
/// number behind the cursor's values, and each page picks up after the
/// last, across a tie the primary key breaks.
#[tokio::test]
async fn a_pager_pages_through_every_row_once() {
    let pool = pool_or_skip!();
    table(
        &pool,
        "pg_pager_test",
        "id bigint primary key, rank bigint not null",
    )
    .await;

    for (id, rank) in [(1i64, 30i64), (2, 10), (3, 20), (4, 20), (5, 40)] {
        sqlx::query("INSERT INTO pg_pager_test VALUES ($1, $2)")
            .bind(id)
            .bind(rank)
            .execute(&pool)
            .await
            .expect("insert");
    }

    let sql = "SELECT id, rank FROM pg_pager_test \
               WHERE /* query.where AND */ TRUE ORDER BY /* query.order_by , */ id \
               LIMIT $1";
    let order_by = OrderByClause::default().asc("rank").asc("id");
    let pager = Pager::new(Cursor::new(order_by.clone()), 2);
    let (mut seen, mut cursor) = (Vec::<Vec<i64>>::new(), None);

    for _ in 0..10 {
        let mut query = QueryComposer::<sqlx::Postgres>::new(sql);
        query
            .bind_value(pager.limit())
            .push_order_by(order_by.clone());
        if let Some(cursor) = cursor.take() {
            query.with_cursor(cursor);
        }

        let rows = query
            .build()
            .expect("composes")
            .fetch_all(&pool)
            .await
            .expect("runs");
        let page = pager.next_page(rows).expect("the row carries every key");

        seen.push(page.rows.iter().map(|row| row.get("id")).collect());
        match page.cursor {
            Some(next) => cursor = Some(next),
            None => break,
        }
    }

    assert_eq!(seen, [vec![2, 3], vec![4, 1], vec![5]]);
}
