//! End-to-end tests against a real Postgres.
//!
//! The unit tests assert what the spliced statement *says*. These assert the
//! part text cannot: that it parses, and that the placeholders line up with the
//! values across the boundary between the statement's own parameters and the
//! fragment's. Both failures produce SQL that reads correctly — the first is
//! caught by the server, the second by nothing at all, since a query bound one
//! slot out still runs and still returns rows.
//!
//! Set `DATABASE_URL` to run them. Without it each test skips, because a
//! missing database is a missing environment rather than a failure:
//!
//! ```sh
//! DATABASE_URL=postgres://localhost/sqlx_query_test cargo test --test postgres
//! ```

use sqlx::{AssertSqlSafe, PgPool, Row};
use sqlx_query::{placeholder_count, shift, splice};

/// The shape sqlc generates: two parameters of its own, and a sentinel before
/// either of them.
const LIST_VOLUMES: &str = "\
SELECT title
FROM volumes
WHERE
    /* query.where AND */ TRUE
ORDER BY
    /* query.order_by , */ id
LIMIT $1 OFFSET $2";

/// Creates `schema`, seeds `volumes` inside it, and returns a pool whose
/// `search_path` points there. Returns `None` when `DATABASE_URL` is unset.
///
/// A schema per test, because `cargo test` runs them concurrently against one
/// database and they would otherwise be seeding the same table.
async fn pool(schema: &str) -> Option<PgPool> {
    let Ok(url) = std::env::var("DATABASE_URL") else {
        // Skipping is right on a machine with no Docker, but in CI it would
        // mean the round trip quietly stopped being tested -- and a skipped
        // test looks exactly like a passing one. The devcontainer is there
        // precisely so this cannot happen, so assert it rather than trust it.
        assert!(
            std::env::var_os("CI").is_none(),
            "DATABASE_URL is unset in CI: the devcontainer's Postgres never \
             reached the shell, so these tests would have silently skipped",
        );
        eprintln!("skipped: DATABASE_URL is unset");
        return None;
    };

    let pool = PgPool::connect(&url)
        .await
        .expect("DATABASE_URL must connect");

    // The schema name is this file's, never a caller's, so the format! is not
    // an injection -- and an identifier cannot be a bind parameter anyway.
    for statement in [
        format!("DROP SCHEMA IF EXISTS {schema} CASCADE"),
        format!("CREATE SCHEMA {schema}"),
        format!("SET search_path TO {schema}"),
        "CREATE TABLE volumes (id BIGSERIAL PRIMARY KEY, title TEXT NOT NULL, read_count INT NOT NULL)".to_owned(),
        "INSERT INTO volumes (title, read_count) VALUES ('Dune', 9), ('Emma', 3), ('Ulysses', 12)"
            .to_owned(),
    ] {
        sqlx::query(AssertSqlSafe(statement))
            .execute(&pool)
            .await
            .expect("seed the schema");
    }

    // `SET` above applies to one pooled connection; this applies to every
    // connection the pool hands out for the rest of the test.
    sqlx::query(AssertSqlSafe(format!(
        "ALTER ROLE CURRENT_USER IN DATABASE {} SET search_path TO {schema}",
        database(&url)
    )))
    .execute(&pool)
    .await
    .ok();

    Some(pool)
}

/// The database name in `url`, for the `ALTER ROLE … IN DATABASE` above.
fn database(url: &str) -> String {
    url.rsplit('/')
        .next()
        .and_then(|tail| tail.split('?').next())
        .unwrap_or("postgres")
        .to_owned()
}

#[tokio::test]
async fn a_spliced_filter_runs_and_selects_what_it_says() {
    let Some(pool) = pool("splice_filter").await else {
        return;
    };

    // The statement binds $1 and $2, so the fragment starts at $3 -- which is
    // the number `placeholder_count` exists to produce.
    let offset = placeholder_count(LIST_VOLUMES) + 1;
    assert_eq!(offset, 3);

    let sql = splice(
        LIST_VOLUMES,
        &[
            ("where", Some(&format!("read_count > ${offset}"))),
            ("order_by", Some("title DESC")),
        ],
    )
    .expect("splice");

    let rows = sqlx::query(AssertSqlSafe(sql))
        .bind(10_i64) // $1, LIMIT
        .bind(0_i64) // $2, OFFSET
        .bind(5_i32) // $3, the fragment's
        .fetch_all(&pool)
        .await
        .expect("the spliced statement must run");

    let titles: Vec<String> = rows.iter().map(|row| row.get("title")).collect();
    assert_eq!(titles, ["Ulysses", "Dune"]);
}

/// The other route to the same statement: a fragment numbered from `$1` and
/// renumbered afterwards. Both paths have to produce the same rows, or one of
/// them is binding a slot out.
#[tokio::test]
async fn a_shifted_fragment_agrees_with_a_pre_numbered_one() {
    let Some(pool) = pool("splice_shift").await else {
        return;
    };

    let shifted = shift("read_count > $1", placeholder_count(LIST_VOLUMES));
    assert_eq!(shifted, "read_count > $3");

    let sql = splice(LIST_VOLUMES, &[("where", Some(&shifted))]).expect("splice");

    let rows = sqlx::query(AssertSqlSafe(sql))
        .bind(10_i64)
        .bind(0_i64)
        .bind(5_i32)
        .fetch_all(&pool)
        .await
        .expect("the spliced statement must run");

    let titles: Vec<String> = rows.iter().map(|row| row.get("title")).collect();
    // Ordered by id, since no order_by fragment was supplied.
    assert_eq!(titles, ["Dune", "Ulysses"]);
}

/// The property the whole convention rests on: unspliced, the statement is the
/// statement. If the sentinels did not survive as comments, generated SQL could
/// not carry them.
#[tokio::test]
async fn an_unspliced_statement_runs_as_written() {
    let Some(pool) = pool("splice_none").await else {
        return;
    };

    let sql = splice(LIST_VOLUMES, &[("where", None), ("order_by", None)]).expect("splice");

    let rows = sqlx::query(AssertSqlSafe(sql))
        .bind(10_i64)
        .bind(1_i64)
        .fetch_all(&pool)
        .await
        .expect("the unspliced statement must run");

    let titles: Vec<String> = rows.iter().map(|row| row.get("title")).collect();
    assert_eq!(titles, ["Emma", "Ulysses"]);
}

/// The sentinel is a comment, so the *original* has to run too — that is what
/// makes it safe to put in checked-in SQL that other tools also read.
#[tokio::test]
async fn the_statement_with_its_sentinels_intact_runs() {
    let Some(pool) = pool("splice_intact").await else {
        return;
    };

    let rows = sqlx::query(AssertSqlSafe(LIST_VOLUMES))
        .bind(10_i64)
        .bind(0_i64)
        .fetch_all(&pool)
        .await
        .expect("the statement must run with its sentinels in place");

    assert_eq!(rows.len(), 3);
}
