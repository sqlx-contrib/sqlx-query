//! Composer-level splice tests, ported from `pgx-contrib/pgxquery`'s
//! `rewriter_test.go` + `fake/*.sql` fixtures, adapted from pgxquery's own
//! connective-first sentinel convention (`/* AND query.where */`) to the
//! name-first convention this crate's regex matches (`/* query.where AND
//! */`) — see DESIGN.md. Each test's *intent* (what pgxquery scenario it
//! ports) is named in its doc comment.
//!
//! These exercise [`QueryComposer::render`] directly: SQL text + bind
//! values, without needing a live database connection.

use sqlx::Execute;
use sqlx_query_v2::{OrderBy, QueryComposer, QueryFragment, Value};

/// A bare [`QueryFragment`] for tests that need to hand the composer raw
/// SQL + values without going through `OrderBy`/CEL parsing.
struct Raw(&'static str, Vec<Value>);

impl QueryFragment for Raw {
    fn into_sql(self) -> (String, Vec<Value>) {
        (self.0.to_owned(), self.1)
    }
}

fn where_fragment() -> Raw {
    Raw("role = 'admin'", Vec::new())
}

fn order_by_fragment() -> Raw {
    Raw("name asc", Vec::new())
}

/// Ports pgxquery's "substitutes where and order_by sentinels" test.
#[test]
fn substitutes_where_and_order_by_sentinels() {
    let sql = "SELECT id FROM users WHERE id = $1 /* query.where AND */ ORDER BY /* query.order_by , */ id LIMIT $2 OFFSET $3";
    let mut query = QueryComposer::<sqlx::Postgres>::new(sql);
    query
        .bind("007")
        .bind(0i64)
        .bind(10i64)
        .filter(where_fragment())
        .order_by(order_by_fragment());

    let (sql, values) = query.render();

    assert!(sql.contains("role = 'admin' AND"));
    assert!(sql.contains("name asc , id"));
    assert!(!sql.contains("query.where"));
    assert!(!sql.contains("query.order_by"));
    assert_eq!(
        values,
        vec![Value::String("007".into()), Value::Int(0), Value::Int(10),]
    );
}

/// Ports pgxquery's "When Where is empty: drops the where sentinel and
/// keeps order_by".
#[test]
fn missing_filter_drops_where_sentinel_and_keeps_order_by() {
    let sql = "SELECT id FROM users WHERE id = $1 /* query.where AND */ ORDER BY /* query.order_by , */ id";
    let mut query = QueryComposer::<sqlx::Postgres>::new(sql);
    query.bind("007").order_by(order_by_fragment());

    let (sql, _) = query.render();

    assert!(!sql.contains("query.where"));
    assert!(!sql.contains("role = 'admin'"));
    assert!(sql.contains("name asc , id"));
}

/// Ports pgxquery's "When OrderBy is empty: drops the order_by sentinel
/// and keeps where".
#[test]
fn missing_order_by_drops_sentinel_and_keeps_where() {
    let sql = "SELECT id FROM users WHERE id = $1 /* query.where AND */ ORDER BY /* query.order_by , */ id";
    let mut query = QueryComposer::<sqlx::Postgres>::new(sql);
    query.bind("007").filter(where_fragment());

    let (sql, _) = query.render();

    assert!(sql.contains("role = 'admin' AND"));
    assert!(!sql.contains("query.order_by"));
    assert!(!sql.contains("name asc"));
}

/// Ports pgxquery's "When both Where and OrderBy are empty: drops both
/// sentinels but still appends Args" — here, the base query's own binds.
#[test]
fn both_missing_drops_both_sentinels_but_keeps_base_binds() {
    let sql = "SELECT id FROM users WHERE id = $1 /* query.where AND */ ORDER BY /* query.order_by , */ id";
    let mut query = QueryComposer::<sqlx::Postgres>::new(sql);
    query.bind("007");

    let (sql, values) = query.render();

    assert!(!sql.contains("query."));
    assert_eq!(values, vec![Value::String("007".into())]);
}

/// Ports pgxquery's "When the sentinel uses OR as the connective".
#[test]
fn preserves_or_connective() {
    let sql = "SELECT * FROM t WHERE a = 1 /* query.where OR */";
    let mut query = QueryComposer::<sqlx::Postgres>::new(sql);
    query.filter(where_fragment());

    let (sql, _) = query.render();

    assert!(sql.contains("role = 'admin' OR"));
    assert!(!sql.contains("query.where"));
}

/// Ports pgxquery's "When the sentinel has no prefix or suffix".
#[test]
fn bare_sentinel_substitutes_value_alone() {
    let sql = "SELECT * FROM t WHERE /* query.where */";
    let mut query = QueryComposer::<sqlx::Postgres>::new(sql);
    query.filter(where_fragment());

    let (sql, _) = query.render();

    assert!(sql.contains("role = 'admin'"));
    assert!(!sql.contains("query.where"));
}

/// Ports pgxquery's "the order_by sentinel is placed before the static
/// list: preserves the trailing comma as the suffix", and its nested
/// "OrderBy is empty: drops the sentinel leaving the static list intact".
#[test]
fn order_by_sentinel_before_static_list() {
    let sql = "SELECT * FROM t ORDER BY /* query.order_by , */ id";

    let mut with_order_by = QueryComposer::<sqlx::Postgres>::new(sql);
    with_order_by.order_by(Raw("priority desc", Vec::new()));
    let (sql_with, _) = with_order_by.render();
    assert!(sql_with.contains("priority desc , id"));

    let without_order_by = QueryComposer::<sqlx::Postgres>::new(sql);
    let (sql_without, _) = without_order_by.render();
    assert!(!sql_without.contains("query.order_by"));
    assert!(!sql_without.contains(','));
    assert!(sql_without.contains("id"));
}

/// Ports pgxquery's "the SQL contains multiple sentinels of the same
/// kind: substitutes each occurrence independently".
#[test]
fn multiple_sentinels_of_the_same_kind_all_substituted() {
    let sql = "SELECT * FROM t WHERE 1 = 1 /* query.where AND */ /* query.where AND */";
    let mut query = QueryComposer::<sqlx::Postgres>::new(sql);
    query.filter(where_fragment());

    let (sql, _) = query.render();

    assert_eq!(sql.matches("role = 'admin' AND").count(), 2);
    assert!(!sql.contains("query.where"));
}

/// Ports pgxquery's "Where uses local placeholders and Args supplies
/// values: shifts placeholders past base args and appends Args".
#[test]
fn shifts_filter_placeholders_past_base_binds() {
    let sql = "SELECT * FROM t WHERE tenant = $1 /* query.where AND */";
    let mut query = QueryComposer::<sqlx::Postgres>::new(sql);
    query.bind("acme").filter(Raw(
        "name = $1 AND score > $2",
        vec![Value::String("alice".into()), Value::Int(90)],
    ));

    let (sql, values) = query.render();

    assert!(sql.contains("name = $2 AND score > $3 AND"));
    assert_eq!(
        values,
        vec![
            Value::String("acme".into()),
            Value::String("alice".into()),
            Value::Int(90),
        ]
    );
}

/// Ports pgxquery's "OrderBy contains a placeholder: shifts the
/// placeholder past the base args" — using 4 base binds, matching the
/// original fixture's offset of 4.
#[test]
fn shifts_order_by_placeholder_past_base_binds() {
    let sql = "SELECT id FROM users WHERE id = $1 /* query.where AND */ ORDER BY /* query.order_by , */ id LIMIT $2 OFFSET $3";
    let mut query = QueryComposer::<sqlx::Postgres>::new(sql);
    query.bind("007").bind(0i64).bind(10i64).order_by(Raw(
        "CASE WHEN role = $1 THEN 0 ELSE 1 END",
        vec![Value::String("admin".into())],
    ));

    let (sql, values) = query.render();

    assert!(sql.contains("CASE WHEN role = $4 THEN 0 ELSE 1 END ,"));
    assert_eq!(
        values,
        vec![
            Value::String("007".into()),
            Value::Int(0),
            Value::Int(10),
            Value::String("admin".into()),
        ]
    );
}

/// Ports pgxquery's "the base query has no positional args (offset is
/// zero): leaves placeholders unchanged".
#[test]
fn zero_offset_leaves_filter_placeholders_unchanged() {
    let sql = "SELECT * FROM t WHERE 1 = 1 /* query.where AND */";
    let mut query = QueryComposer::<sqlx::Postgres>::new(sql);
    query.filter(Raw(
        "name = $1 AND score > $2",
        vec![Value::String("alice".into()), Value::Int(90)],
    ));

    let (sql, values) = query.render();

    assert!(sql.contains("name = $1 AND score > $2 AND"));
    assert_eq!(values, vec![Value::String("alice".into()), Value::Int(90)]);
}

/// Ports pgxquery's "a non-matching comment is present alongside a
/// sentinel: leaves the regular comment untouched".
#[test]
fn leaves_non_matching_comments_untouched() {
    let sql = "SELECT 1 /* regular comment */ FROM t WHERE TRUE /* query.where AND */";
    let mut query = QueryComposer::<sqlx::Postgres>::new(sql);
    query.filter(where_fragment());

    let (sql, _) = query.render();

    assert!(sql.contains("/* regular comment */"));
    assert!(sql.contains("role = 'admin' AND"));
}

/// Ports pgxquery's "an unknown sentinel name is used: drops the sentinel
/// entirely".
#[test]
fn unknown_sentinel_name_dropped_entirely() {
    let sql = "SELECT 1 FROM t WHERE TRUE /* query.unknown AND */";
    let query = QueryComposer::<sqlx::Postgres>::new(sql);

    let (sql, _) = query.render();

    assert!(!sql.contains("query.unknown"));
    assert!(!sql.contains("AND"));
}

/// Ports pgxquery's "the SQL has no sentinels but Args is set: appends
/// Args to the positional args" — here, base binds with no sentinels
/// present at all.
#[test]
fn no_sentinels_present_leaves_sql_untouched() {
    let sql = "SELECT * FROM t WHERE id = $1";
    let mut query = QueryComposer::<sqlx::Postgres>::new(sql);
    query.bind(1i64);

    let (rendered, values) = query.render();

    assert_eq!(rendered, sql);
    assert_eq!(values, vec![Value::Int(1)]);
}

/// The scenario DESIGN.md calls out explicitly: a marker with
/// caller-numbered placeholders *after* it in the text (`LIMIT $1 OFFSET
/// $2`), shaped like the real grpc-rust-template target queries. Proves
/// numbering is index-based (by bind-declaration order), not
/// text-order-based — the filter/order_by fragments' own placeholders
/// still get shifted past `take`/`skip` even though those appear later in
/// the SQL text.
#[test]
fn index_based_numbering_survives_placeholders_declared_after_the_marker_in_text() {
    let sql = "SELECT * FROM collection \
               WHERE /* query.where AND */ TRUE \
               ORDER BY /* query.order_by , */ collection_id \
               LIMIT $1 OFFSET $2";

    let mut query = QueryComposer::<sqlx::Postgres>::new(sql);
    query
        .bind(50i64) // take -> $1
        .bind(0i64) // skip -> $2
        .filter(Raw("status = $1", vec![Value::String("ACTIVE".into())]))
        .order_by(Raw("rank = $1", vec![Value::Int(7)]));

    let (sql, values) = query.render();

    assert!(sql.contains("WHERE status = $3 AND TRUE"));
    assert!(sql.contains("ORDER BY rank = $4 , collection_id"));
    assert!(sql.contains("LIMIT $1 OFFSET $2"));
    assert_eq!(
        values,
        vec![
            Value::Int(50),
            Value::Int(0),
            Value::String("ACTIVE".into()),
            Value::Int(7),
        ]
    );
}

/// `OrderBy::into_sql` end to end through the composer (not just `Raw`),
/// proving the real `QueryFragment` impl splices correctly too.
#[test]
fn order_by_fragment_type_splices_through_composer() {
    let sql = "SELECT * FROM t ORDER BY /* query.order_by , */ id";
    let order_by = OrderBy::parse("rank desc").unwrap();

    let mut query = QueryComposer::<sqlx::Postgres>::new(sql);
    query.order_by(order_by);

    let (sql, values) = query.render();

    assert!(sql.contains("rank DESC , id"));
    assert!(values.is_empty());
}

/// `build()` actually produces an executable `sqlx::query::Query` (not
/// just testing `render()` in isolation) for each dialect this crate
/// implements `QueryDialect` for.
#[test]
fn build_produces_a_query_with_the_rendered_sql() {
    let sql = "SELECT * FROM t WHERE /* query.where AND */ TRUE";
    let mut query = QueryComposer::<sqlx::Postgres>::new(sql);
    query.filter(where_fragment());

    let built = query.build().unwrap();

    assert!(built.sql().contains("role = 'admin' AND TRUE"));
}
