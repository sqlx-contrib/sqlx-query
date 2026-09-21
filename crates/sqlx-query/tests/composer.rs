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
use sqlx_query::{Cursor, Error, OrderClause, QueryComposer, Value, WhereClause};

fn admin_filter() -> WhereClause {
    WhereClause::new("role = 'admin'")
}

fn name_order_by() -> OrderClause {
    OrderClause::parse("name asc").unwrap()
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
        .push_where(admin_filter())
        .push_order(name_order_by());

    let (sql, values) = query.render().unwrap();

    assert!(sql.contains("role = 'admin' AND"));
    assert!(sql.contains("name ASC , id"));
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
    query.bind("007").push_order(name_order_by());

    let (sql, _) = query.render().unwrap();

    assert!(!sql.contains("query.where"));
    assert!(!sql.contains("role = 'admin'"));
    assert!(sql.contains("name ASC , id"));
}

/// Ports pgxquery's "When OrderClause is empty: drops the order_by sentinel
/// and keeps where".
#[test]
fn missing_order_by_drops_sentinel_and_keeps_where() {
    let sql = "SELECT id FROM users WHERE id = $1 /* query.where AND */ ORDER BY /* query.order_by , */ id";
    let mut query = QueryComposer::<sqlx::Postgres>::new(sql);
    query.bind("007").push_where(admin_filter());

    let (sql, _) = query.render().unwrap();

    assert!(sql.contains("role = 'admin' AND"));
    assert!(!sql.contains("query.order_by"));
    assert!(!sql.contains("ASC"));
}

/// Ports pgxquery's "When both Where and OrderClause are empty: drops both
/// sentinels but still appends Args" — here, the base query's own binds.
#[test]
fn both_missing_drops_both_sentinels_but_keeps_base_binds() {
    let sql = "SELECT id FROM users WHERE id = $1 /* query.where AND */ ORDER BY /* query.order_by , */ id";
    let mut query = QueryComposer::<sqlx::Postgres>::new(sql);
    query.bind("007");

    let (sql, values) = query.render().unwrap();

    assert!(!sql.contains("query."));
    assert_eq!(values, vec![Value::String("007".into())]);
}

/// Ports pgxquery's "When the sentinel uses OR as the connective".
#[test]
fn preserves_or_connective() {
    let sql = "SELECT * FROM t WHERE a = 1 /* query.where OR */";
    let mut query = QueryComposer::<sqlx::Postgres>::new(sql);
    query.push_where(admin_filter());

    let (sql, _) = query.render().unwrap();

    assert!(sql.contains("role = 'admin' OR"));
    assert!(!sql.contains("query.where"));
}

/// Ports pgxquery's "When the sentinel has no prefix or suffix".
#[test]
fn bare_sentinel_substitutes_value_alone() {
    let sql = "SELECT * FROM t WHERE /* query.where */";
    let mut query = QueryComposer::<sqlx::Postgres>::new(sql);
    query.push_where(admin_filter());

    let (sql, _) = query.render().unwrap();

    assert!(sql.contains("role = 'admin'"));
    assert!(!sql.contains("query.where"));
}

/// Ports pgxquery's "the order_by sentinel is placed before the static
/// list: preserves the trailing comma as the suffix", and its nested
/// "OrderClause is empty: drops the sentinel leaving the static list intact".
#[test]
fn order_by_sentinel_before_static_list() {
    let sql = "SELECT * FROM t ORDER BY /* query.order_by , */ id";

    let mut with_order_by = QueryComposer::<sqlx::Postgres>::new(sql);
    with_order_by.push_order(OrderClause::parse("priority desc").unwrap());
    let (sql_with, _) = with_order_by.render().unwrap();
    assert!(sql_with.contains("priority DESC , id"));

    let without_order_by = QueryComposer::<sqlx::Postgres>::new(sql);
    let (sql_without, _) = without_order_by.render().unwrap();
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
    query.push_where(admin_filter());

    let (sql, _) = query.render().unwrap();

    assert_eq!(sql.matches("role = 'admin' AND").count(), 2);
    assert!(!sql.contains("query.where"));
}

/// Ports pgxquery's "Where uses local placeholders and Args supplies
/// values: shifts placeholders past base args and appends Args".
#[test]
fn shifts_filter_placeholders_past_base_binds() {
    let sql = "SELECT * FROM t WHERE tenant = $1 /* query.where AND */";
    let mut query = QueryComposer::<sqlx::Postgres>::new(sql);
    query.bind("acme").push_where(
        WhereClause::new("name = $1 AND score > $2")
            .bind("alice")
            .bind(90i64),
    );

    let (sql, values) = query.render().unwrap();

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

/// `OrderClause` never carries bind values (it only ever renders column
/// names and directions), so unlike `WhereClause`, its sentinel never needs
/// placeholder shifting — this documents that invariant, replacing
/// pgxquery's "OrderBy contains a placeholder" scenario, which doesn't
/// apply to the concrete `OrderClause` type this crate uses.
#[test]
fn order_by_never_contributes_bind_values() {
    let sql = "SELECT id FROM users WHERE id = $1 /* query.where AND */ ORDER BY /* query.order_by , */ id LIMIT $2 OFFSET $3";
    let mut query = QueryComposer::<sqlx::Postgres>::new(sql);
    query
        .bind("007")
        .bind(0i64)
        .bind(10i64)
        .push_order(OrderClause::parse("rank desc").unwrap());

    let (sql, values) = query.render().unwrap();

    assert!(sql.contains("rank DESC , id"));
    assert_eq!(
        values,
        vec![Value::String("007".into()), Value::Int(0), Value::Int(10)]
    );
}

/// Ports pgxquery's "the base query has no positional args (offset is
/// zero): leaves placeholders unchanged".
#[test]
fn zero_offset_leaves_filter_placeholders_unchanged() {
    let sql = "SELECT * FROM t WHERE 1 = 1 /* query.where AND */";
    let mut query = QueryComposer::<sqlx::Postgres>::new(sql);
    query.push_where(
        WhereClause::new("name = $1 AND score > $2")
            .bind("alice")
            .bind(90i64),
    );

    let (sql, values) = query.render().unwrap();

    assert!(sql.contains("name = $1 AND score > $2 AND"));
    assert_eq!(values, vec![Value::String("alice".into()), Value::Int(90)]);
}

/// Ports pgxquery's "a non-matching comment is present alongside a
/// sentinel: leaves the regular comment untouched".
#[test]
fn leaves_non_matching_comments_untouched() {
    let sql = "SELECT 1 /* regular comment */ FROM t WHERE TRUE /* query.where AND */";
    let mut query = QueryComposer::<sqlx::Postgres>::new(sql);
    query.push_where(admin_filter());

    let (sql, _) = query.render().unwrap();

    assert!(sql.contains("/* regular comment */"));
    assert!(sql.contains("role = 'admin' AND"));
}

/// Ports pgxquery's "an unknown sentinel name is used: drops the sentinel
/// entirely".
#[test]
fn unknown_sentinel_name_dropped_entirely() {
    let sql = "SELECT 1 FROM t WHERE TRUE /* query.unknown AND */";
    let query = QueryComposer::<sqlx::Postgres>::new(sql);

    let (sql, _) = query.render().unwrap();

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

    let (rendered, values) = query.render().unwrap();

    assert_eq!(rendered, sql);
    assert_eq!(values, vec![Value::Int(1)]);
}

/// The scenario DESIGN.md calls out explicitly: a marker with
/// caller-numbered placeholders *after* it in the text (`LIMIT $1 OFFSET
/// $2`), shaped like the real grpc-rust-template target queries. Proves
/// numbering is index-based (by bind-declaration order), not
/// text-order-based — the `where_by` value's own placeholder still gets
/// shifted past `take`/`skip` even though those appear later in the SQL
/// text.
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
        .push_where(WhereClause::new("status = $1").bind("ACTIVE"))
        .push_order(OrderClause::parse("rank desc").unwrap());

    let (sql, values) = query.render().unwrap();

    assert!(sql.contains("WHERE status = $3 AND TRUE"));
    assert!(sql.contains("ORDER BY rank DESC , collection_id"));
    assert!(sql.contains("LIMIT $1 OFFSET $2"));
    assert_eq!(
        values,
        vec![
            Value::Int(50),
            Value::Int(0),
            Value::String("ACTIVE".into())
        ]
    );
}

/// `OrderClause` end to end through the composer, using real column
/// resolution rather than a hand-built `WhereClause`.
#[test]
fn order_by_splices_through_composer() {
    let sql = "SELECT * FROM t ORDER BY /* query.order_by , */ id";
    let order_by = OrderClause::parse("rank desc").unwrap();

    let mut query = QueryComposer::<sqlx::Postgres>::new(sql);
    query.push_order(order_by);

    let (sql, values) = query.render().unwrap();

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
    query.push_where(admin_filter());

    let built = query.build().unwrap();

    assert!(built.sql().as_str().contains("role = 'admin' AND TRUE"));
}

fn rank_cursor() -> Cursor {
    let order_by = OrderClause::parse("rank desc, id asc").unwrap();
    Cursor::new(order_by)
        .after(vec![Value::Int(42), Value::Int(7)])
        .unwrap()
}

/// A cursor alone (no separate `.push_order()` call) supplies its own
/// order_by directly — this is the expected, ergonomic case, not a
/// degraded fallback.
#[test]
fn cursor_alone_supplies_its_own_order_by() {
    let sql = "SELECT * FROM t WHERE /* query.where AND */ TRUE ORDER BY /* query.order_by , */ id";
    let mut query = QueryComposer::<sqlx::Postgres>::new(sql);
    query.cursor(rank_cursor());

    let (sql, values) = query.render().unwrap();

    assert!(sql.contains("WHERE (rank < $1) OR (rank = $1 AND id > $2) AND TRUE"));
    assert!(sql.contains("ORDER BY rank DESC, id ASC , id"));
    assert_eq!(values, vec![Value::Int(42), Value::Int(7)]);
}

/// A cursor whose order_by matches an explicitly-set `.push_order()` is
/// accepted — the common case of a client re-sending the same sort on
/// every page.
#[test]
fn cursor_with_matching_order_by_is_accepted() {
    let sql = "SELECT * FROM t WHERE /* query.where AND */ TRUE ORDER BY /* query.order_by , */ id";
    let mut query = QueryComposer::<sqlx::Postgres>::new(sql);
    query
        .cursor(rank_cursor())
        .push_order(OrderClause::parse("rank desc, id asc").unwrap());

    assert!(query.render().is_ok());
}

/// A cursor whose order_by no longer matches the request's current
/// order_by (client changed their sort mid-pagination) is rejected.
#[test]
fn cursor_with_mismatched_order_by_is_rejected() {
    let sql = "SELECT * FROM t WHERE /* query.where AND */ TRUE ORDER BY /* query.order_by , */ id";
    let mut query = QueryComposer::<sqlx::Postgres>::new(sql);
    query
        .cursor(rank_cursor())
        .push_order(OrderClause::parse("id asc").unwrap());

    assert!(matches!(
        query.render().unwrap_err(),
        Error::CursorOrderByMismatch
    ));
}

/// A cursor and a plain filter both apply — pagination must not silently
/// drop a filter the caller also set.
#[test]
fn cursor_and_filter_are_combined_with_and() {
    let sql = "SELECT * FROM t WHERE /* query.where AND */ TRUE";
    let mut query = QueryComposer::<sqlx::Postgres>::new(sql);
    query
        .push_where(WhereClause::new("status = $1").bind("ACTIVE"))
        .cursor(rank_cursor());

    let (sql, values) = query.render().unwrap();

    assert!(
        sql.contains("WHERE (status = $1) AND ((rank < $2) OR (rank = $2 AND id > $3)) AND TRUE")
    );
    assert_eq!(
        values,
        vec![
            Value::String("ACTIVE".into()),
            Value::Int(42),
            Value::Int(7),
        ]
    );
}

/// Non-positional dialects (`?`) have no placeholder numbering to shift —
/// deciding whether to shift at all is the composer's call
/// (`WhereClause::shift` itself always shifts unconditionally).
#[test]
fn non_positional_dialects_do_not_shift_where_by_placeholders() {
    let sql = "SELECT * FROM t WHERE tenant = ? /* query.where AND */";
    let mut query = QueryComposer::<sqlx::Sqlite>::new(sql);
    query
        .bind("acme")
        .push_where(WhereClause::new("name = ?").bind("alice"));

    let (sql, values) = query.render().unwrap();

    assert!(sql.contains("name = ? AND"));
    assert_eq!(
        values,
        vec![Value::String("acme".into()), Value::String("alice".into())]
    );
}

/// Multiple `.push_where()` calls accumulate (AND together) instead of the
/// last one replacing the others — e.g. an always-present tenant scope
/// plus a client-supplied filter.
#[test]
fn where_by_accumulates_across_multiple_calls() {
    let sql = "SELECT * FROM t WHERE /* query.where AND */ TRUE";
    let mut query = QueryComposer::<sqlx::Postgres>::new(sql);
    query
        .push_where(WhereClause::new("tenant_id = $1").bind("acme"))
        .push_where(WhereClause::new("status = $1").bind("ACTIVE"));

    let (sql, values) = query.render().unwrap();

    assert!(sql.contains("WHERE (tenant_id = $1) AND (status = $2) AND TRUE"));
    assert_eq!(
        values,
        vec![Value::String("acme".into()), Value::String("ACTIVE".into())]
    );
}

/// Multiple `.push_order()` calls accumulate as tie-breakers in call
/// order, not the last one replacing the others.
#[test]
fn push_order_accumulates_as_tie_breakers_in_call_order() {
    let sql = "SELECT * FROM t ORDER BY /* query.order_by , */ id";
    let mut query = QueryComposer::<sqlx::Postgres>::new(sql);
    query
        .push_order(OrderClause::parse("tenant_id asc").unwrap())
        .push_order(OrderClause::parse("rank desc").unwrap());

    let (sql, _) = query.render().unwrap();

    assert!(sql.contains("ORDER BY tenant_id ASC, rank DESC , id"));
}

/// When an explicit accumulated order_by matches the cursor's, the
/// cursor's order_by isn't appended as an extra tie-breaker on top of it
/// — the two are checked for equality, not concatenated.
#[test]
fn explicit_order_by_is_not_duplicated_by_a_matching_cursor() {
    let sql = "SELECT * FROM t ORDER BY /* query.order_by , */ id";
    let mut query = QueryComposer::<sqlx::Postgres>::new(sql);
    query
        .push_order(OrderClause::parse("rank desc, id asc").unwrap())
        .cursor(rank_cursor());

    let (sql, _) = query.render().unwrap();

    assert!(sql.contains("ORDER BY rank DESC, id ASC , id"));
    assert_eq!(sql.matches("rank DESC").count(), 1);
}
