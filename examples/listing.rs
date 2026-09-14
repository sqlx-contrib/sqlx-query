//! A list endpoint, on both drivers.
//!
//! The base query is written once. What a request asked for -- a filter, a
//! sort, a page size -- is layered on top of it, and only the type parameter
//! says which database the result is for.
//!
//! Run with: `cargo run --example listing --features postgres,sqlite`

use sqlx::{Postgres, Sqlite, SqlitePool};
use sqlx_query::{Error, QueryWriter, Syntax};

/// The query you already wrote. Plain SQL: it runs in psql, it EXPLAINs, and
/// `sqlx::query!` will check it against a live database.
///
/// `$1` parses for both drivers here, so one string serves both. Neither ever
/// sees it -- PostgreSQL is sent `$1` and SQLite `?1`.
const VOLUMES: &str = "SELECT id, title, read_count \
                       FROM volumes \
                       WHERE tenant_id = $1 \
                       ORDER BY id";

/// What a request asked for. Everything but the tenant is optional, which is
/// the whole reason the query has to be assembled rather than written out.
#[derive(Default)]
struct Listing<'a> {
    filter: Option<&'a str>,
    order: Option<&'a str>,
    limit: Option<u64>,
}

/// Lays a request over the base query. Generic over the driver: the same code
/// serves both, and the placeholders come out in each one's own form.
///
/// The `i64` bounds are sqlx's, not this crate's -- any code generic over a
/// `Database` has to say which types it intends to bind. Most applications
/// target one database and name it directly, which needs none of this.
fn assemble<DB>(tenant: i64, request: &Listing<'_>) -> Result<QueryWriter<DB>, Error>
where
    DB: Syntax,
    i64: for<'t> sqlx::Encode<'t, DB> + sqlx::Type<DB>,
{
    let mut writer = QueryWriter::<DB>::new(VOLUMES)?;
    writer.bind(tenant);

    if let Some(filter) = request.filter {
        writer.and_where(filter);
    }
    if let Some(order) = request.order {
        writer.order_by(order);
    }
    if let Some(limit) = request.limit {
        writer.limit(limit);
    }

    Ok(writer)
}

#[derive(sqlx::FromRow)]
struct Volume {
    id: i64,
    title: String,
    read_count: i64,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let requests = [
        ("nothing asked for", Listing::default()),
        (
            "a filter",
            Listing {
                filter: Some("read_count > 100"),
                ..Listing::default()
            },
        ),
        (
            "a filter and a sort",
            Listing {
                filter: Some("read_count > 100"),
                order: Some("title desc"),
                ..Listing::default()
            },
        ),
        (
            "a bound value in the filter",
            Listing {
                filter: Some("title LIKE $1"),
                order: Some("read_count desc"),
                limit: Some(20),
            },
        ),
    ];

    for (what, request) in &requests {
        println!("\n--- {what}");
        println!("  postgres  {}", assemble::<Postgres>(7, request)?.sql()?);
        println!("  sqlite    {}", assemble::<Sqlite>(7, request)?.sql()?);
    }

    // --- and the same thing actually run -------------------------------
    let pool = SqlitePool::connect("sqlite::memory:").await?;
    sqlx::query("CREATE TABLE volumes (id INTEGER PRIMARY KEY, tenant_id INTEGER, title TEXT, read_count INTEGER)")
        .execute(&pool)
        .await?;
    for (id, tenant, title, reads) in [
        (1, 7, "Dune", 940),
        (2, 7, "Deep Work", 120),
        (3, 7, "Solaris", 40),
        (4, 9, "Ubik", 800),
    ] {
        sqlx::query("INSERT INTO volumes VALUES (?, ?, ?, ?)")
            .bind(id)
            .bind(tenant)
            .bind(title)
            .bind(reads)
            .execute(&pool)
            .await?;
    }

    println!("\n--- executed on sqlite");
    let mut writer = assemble::<Sqlite>(
        7,
        &Listing {
            filter: Some("title LIKE $1"),
            order: Some("read_count desc"),
            limit: Some(20),
        },
    )?;
    writer.bind("D%"); // the filter's own value, bound after the base query's

    println!("  sql   {}", writer.sql()?);
    for volume in writer.build_as::<Volume>()?.fetch_all(&pool).await? {
        println!(
            "  row   {:>2}  {:<12} {} reads",
            volume.id, volume.title, volume.read_count
        );
    }

    Ok(())
}
