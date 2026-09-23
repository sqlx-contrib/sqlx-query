//! Composer-level splice tests, ported from `pgx-contrib/pgxquery`'s
//! `rewriter_test.go` + `fake/*.sql` fixtures, adapted from pgxquery's own
//! connective-first slot convention (`/* AND query.where */`) to the
//! name-first convention this crate's regex matches (`/* query.where AND
//! */`). Each test's *intent* (what pgxquery scenario it ports) is named
//! in its doc comment.
//!
//! These exercise [`QueryComposer::compose`] directly: SQL text + bind
//! values, without needing a live database connection.

#![cfg(all(feature = "postgres", feature = "mysql", feature = "sqlite"))]

use sqlx::Execute;
use sqlx_query::{Cursor, Error, OrderByClause, QueryComposer, QueryStatement, Value, WhereClause};

/// Every test here supplies its parameters through `bind_value`, so every
/// argument is a visible scalar. This unwraps that once rather than making
/// each assertion carry `Some(...)` -- and it fails loudly if a test ever
/// starts using `bind`, whose values the composer deliberately can't show.
fn values<DB: sqlx::Database>(statement: &QueryStatement<DB>) -> Vec<Value> {
    statement
        .arguments()
        .iter()
        .map(|argument| {
            argument
                .value()
                .expect("these tests bind scalars, not opaque values")
                .clone()
        })
        .collect()
}

fn admin_filter() -> WhereClause {
    WhereClause::new("role = 'admin'")
}

fn name_order_by() -> OrderByClause {
    OrderByClause::parse("name asc").unwrap()
}

/// Ports pgxquery's "substitutes where and `order_by` slots" test.
#[test]
fn substitutes_where_and_order_by_slots() {
    let sql = "SELECT id FROM users WHERE id = $1 /* query.where AND */ ORDER BY /* query.order_by , */ id LIMIT $2 OFFSET $3";
    let mut query = QueryComposer::<sqlx::Postgres>::new(sql);
    query
        .bind_value("007")
        .bind_value(0i64)
        .bind_value(10i64)
        .push_where(admin_filter())
        .push_order_by(name_order_by());

    let statement = query.compose().unwrap();
    let (sql, values) = (statement.sql(), values(&statement));

    assert!(sql.contains("role = 'admin' AND"));
    assert!(sql.contains("name ASC , id"));
    assert!(!sql.contains("query.where"));
    assert!(!sql.contains("query.order_by"));
    assert_eq!(
        values,
        vec![Value::String("007".into()), Value::Int(0), Value::Int(10),]
    );
}

/// Ports pgxquery's "When Where is empty: drops the where slot and
/// keeps `order_by`".
#[test]
fn missing_filter_drops_where_slot_and_keeps_order_by() {
    let sql = "SELECT id FROM users WHERE id = $1 /* query.where AND */ ORDER BY /* query.order_by , */ id";
    let mut query = QueryComposer::<sqlx::Postgres>::new(sql);
    query.bind_value("007").push_order_by(name_order_by());

    let sql = query.compose().unwrap().sql().to_owned();

    assert!(!sql.contains("query.where"));
    assert!(!sql.contains("role = 'admin'"));
    assert!(sql.contains("name ASC , id"));
}

/// Ports pgxquery's "When `OrderByClause` is empty: drops the `order_by` slot
/// and keeps where".
#[test]
fn missing_order_by_drops_slot_and_keeps_where() {
    let sql = "SELECT id FROM users WHERE id = $1 /* query.where AND */ ORDER BY /* query.order_by , */ id";
    let mut query = QueryComposer::<sqlx::Postgres>::new(sql);
    query.bind_value("007").push_where(admin_filter());

    let sql = query.compose().unwrap().sql().to_owned();

    assert!(sql.contains("role = 'admin' AND"));
    assert!(!sql.contains("query.order_by"));
    assert!(!sql.contains("ASC"));
}

/// Ports pgxquery's "When both `Where` and `OrderByClause` are empty: drops both
/// slots but still appends Args" — here, the base query's own binds.
#[test]
fn both_missing_drops_both_slots_but_keeps_base_binds() {
    let sql = "SELECT id FROM users WHERE id = $1 /* query.where AND */ ORDER BY /* query.order_by , */ id";
    let mut query = QueryComposer::<sqlx::Postgres>::new(sql);
    query.bind_value("007");

    let statement = query.compose().unwrap();
    let (sql, values) = (statement.sql(), values(&statement));

    assert!(!sql.contains("query."));
    assert_eq!(values, vec![Value::String("007".into())]);
}

/// Ports pgxquery's "When the slot uses OR as the connective".
#[test]
fn preserves_or_connective() {
    let sql = "SELECT * FROM t WHERE a = 1 /* query.where OR */";
    let mut query = QueryComposer::<sqlx::Postgres>::new(sql);
    query.push_where(admin_filter());

    let sql = query.compose().unwrap().sql().to_owned();

    assert!(sql.contains("role = 'admin' OR"));
    assert!(!sql.contains("query.where"));
}

/// Ports pgxquery's "When the slot has no prefix or suffix".
#[test]
fn bare_slot_substitutes_value_alone() {
    let sql = "SELECT * FROM t WHERE /* query.where */";
    let mut query = QueryComposer::<sqlx::Postgres>::new(sql);
    query.push_where(admin_filter());

    let sql = query.compose().unwrap().sql().to_owned();

    assert!(sql.contains("role = 'admin'"));
    assert!(!sql.contains("query.where"));
}

/// Ports pgxquery's "the `order_by` slot is placed before the static
/// list: preserves the trailing comma as the suffix", and its nested
/// "`OrderByClause` is empty: drops the slot leaving the static list intact".
#[test]
fn order_by_slot_before_static_list() {
    let sql = "SELECT * FROM t ORDER BY /* query.order_by , */ id";

    let mut with_order_by = QueryComposer::<sqlx::Postgres>::new(sql);
    with_order_by.push_order_by(OrderByClause::parse("priority desc").unwrap());
    let sql_with = with_order_by.compose().unwrap().sql().to_owned();
    assert!(sql_with.contains("priority DESC , id"));

    let without_order_by = QueryComposer::<sqlx::Postgres>::new(sql);
    let sql_without = without_order_by.compose().unwrap().sql().to_owned();
    assert!(!sql_without.contains("query.order_by"));
    assert!(!sql_without.contains(','));
    assert!(sql_without.contains("id"));
}

/// Ports pgxquery's "the SQL contains multiple slots of the same
/// kind: substitutes each occurrence independently".
#[test]
fn multiple_slots_of_the_same_kind_all_substituted() {
    let sql = "SELECT * FROM t WHERE 1 = 1 /* query.where AND */ /* query.where AND */";
    let mut query = QueryComposer::<sqlx::Postgres>::new(sql);
    query.push_where(admin_filter());

    let sql = query.compose().unwrap().sql().to_owned();

    assert_eq!(sql.matches("role = 'admin' AND").count(), 2);
    assert!(!sql.contains("query.where"));
}

/// Ports pgxquery's "Where uses local placeholders and Args supplies
/// values: shifts placeholders past base args and appends Args".
#[test]
fn shifts_filter_placeholders_past_base_binds() {
    let sql = "SELECT * FROM t WHERE tenant = $1 /* query.where AND */";
    let mut query = QueryComposer::<sqlx::Postgres>::new(sql);
    query.bind_value("acme").push_where(
        WhereClause::new("name = $1 AND score > $2")
            .bind_value("alice")
            .bind_value(90i64),
    );

    let statement = query.compose().unwrap();
    let (sql, values) = (statement.sql(), values(&statement));

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

/// `OrderByClause` never carries bind values (it only ever renders column
/// names and directions), so unlike `WhereClause`, its slot never needs
/// placeholder shifting — this documents that invariant, replacing
/// pgxquery's "`OrderBy` contains a placeholder" scenario, which doesn't
/// apply to the concrete `OrderByClause` type this crate uses.
#[test]
fn order_by_never_contributes_bind_values() {
    let sql = "SELECT id FROM users WHERE id = $1 /* query.where AND */ ORDER BY /* query.order_by , */ id LIMIT $2 OFFSET $3";
    let mut query = QueryComposer::<sqlx::Postgres>::new(sql);
    query
        .bind_value("007")
        .bind_value(0i64)
        .bind_value(10i64)
        .push_order_by(OrderByClause::parse("rank desc").unwrap());

    let statement = query.compose().unwrap();
    let (sql, values) = (statement.sql(), values(&statement));

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
            .bind_value("alice")
            .bind_value(90i64),
    );

    let statement = query.compose().unwrap();
    let (sql, values) = (statement.sql(), values(&statement));

    assert!(sql.contains("name = $1 AND score > $2 AND"));
    assert_eq!(values, vec![Value::String("alice".into()), Value::Int(90)]);
}

/// Ports pgxquery's "a non-matching comment is present alongside a
/// slot: leaves the regular comment untouched".
#[test]
fn leaves_non_matching_comments_untouched() {
    let sql = "SELECT 1 /* regular comment */ FROM t WHERE TRUE /* query.where AND */";
    let mut query = QueryComposer::<sqlx::Postgres>::new(sql);
    query.push_where(admin_filter());

    let sql = query.compose().unwrap().sql().to_owned();

    assert!(sql.contains("/* regular comment */"));
    assert!(sql.contains("role = 'admin' AND"));
}

/// Ports pgxquery's "an unknown slot name is used: drops the slot
/// entirely".
#[test]
fn unknown_slot_name_dropped_entirely() {
    let sql = "SELECT 1 FROM t WHERE TRUE /* query.unknown AND */";
    let query = QueryComposer::<sqlx::Postgres>::new(sql);

    let sql = query.compose().unwrap().sql().to_owned();

    assert!(!sql.contains("query.unknown"));
    assert!(!sql.contains("AND"));
}

/// Ports pgxquery's "the SQL has no slots but Args is set: appends
/// Args to the positional args" — here, base binds with no slots
/// present at all.
#[test]
fn no_slots_present_leaves_sql_untouched() {
    let sql = "SELECT * FROM t WHERE id = $1";
    let mut query = QueryComposer::<sqlx::Postgres>::new(sql);
    query.bind_value(1i64);

    let statement = query.compose().unwrap();
    let (rendered, values) = (statement.sql(), values(&statement));

    assert_eq!(rendered, sql);
    assert_eq!(values, vec![Value::Int(1)]);
}

/// A marker with caller-numbered placeholders *after* it in the text
/// (`LIMIT $1 OFFSET $2`), shaped like the real grpc-rust-template target
/// queries. Proves numbering is index-based (by bind-declaration order),
/// not text-order-based — the `push_where` value's own placeholder still
/// gets shifted past `take`/`skip` even though those appear later in the
/// SQL text.
#[test]
fn index_based_numbering_survives_placeholders_declared_after_the_marker_in_text() {
    let sql = "SELECT * FROM collection \
               WHERE /* query.where AND */ TRUE \
               ORDER BY /* query.order_by , */ collection_id \
               LIMIT $1 OFFSET $2";

    let mut query = QueryComposer::<sqlx::Postgres>::new(sql);
    query
        .bind_value(50i64) // take -> $1
        .bind_value(0i64) // skip -> $2
        .push_where(WhereClause::new("status = $1").bind_value("ACTIVE"))
        .push_order_by(OrderByClause::parse("rank desc").unwrap());

    let statement = query.compose().unwrap();
    let (sql, values) = (statement.sql(), values(&statement));

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

/// `OrderByClause` end to end through the composer, using real column
/// resolution rather than a hand-built `WhereClause`.
#[test]
fn order_by_splices_through_composer() {
    let sql = "SELECT * FROM t ORDER BY /* query.order_by , */ id";
    let order_by = OrderByClause::parse("rank desc").unwrap();

    let mut query = QueryComposer::<sqlx::Postgres>::new(sql);
    query.push_order_by(order_by);

    let statement = query.compose().unwrap();
    let (sql, values) = (statement.sql(), values(&statement));

    assert!(sql.contains("rank DESC , id"));
    assert!(values.is_empty());
}

/// `build()` actually produces an executable `sqlx::query::Query` (not
/// just testing `compose()` in isolation) for each dialect this crate
/// implements `QueryDialect` for.
#[test]
fn build_produces_a_query_with_the_composed_sql() {
    let sql = "SELECT * FROM t WHERE /* query.where AND */ TRUE";
    let mut query = QueryComposer::<sqlx::Postgres>::new(sql);
    query.push_where(admin_filter());

    let built = query.build().unwrap();

    assert!(built.sql().as_str().contains("role = 'admin' AND TRUE"));
}

fn rank_cursor() -> Cursor {
    let order_by = OrderByClause::parse("rank desc, id asc").unwrap();
    Cursor::new(order_by)
        .after(vec![Value::Int(42), Value::Int(7)])
        .unwrap()
}

/// A cursor alone (no separate `.push_order_by()` call) supplies its own
/// `order_by` directly — this is the expected, ergonomic case, not a
/// degraded fallback.
#[test]
fn cursor_alone_supplies_its_own_order_by() {
    let sql = "SELECT * FROM t WHERE /* query.where AND */ TRUE ORDER BY /* query.order_by , */ id";
    let mut query = QueryComposer::<sqlx::Postgres>::new(sql);
    query.with_cursor(rank_cursor());

    let statement = query.compose().unwrap();
    let (sql, values) = (statement.sql(), values(&statement));

    assert!(sql.contains("WHERE (rank < $1) OR (rank = $1 AND id > $2) AND TRUE"));
    assert!(sql.contains("ORDER BY rank DESC, id ASC , id"));
    assert_eq!(values, vec![Value::Int(42), Value::Int(7)]);
}

/// A cursor whose `order_by` matches an explicitly-set `.push_order_by()` is
/// accepted — the common case of a client re-sending the same sort on
/// every page.
#[test]
fn cursor_with_matching_order_by_is_accepted() {
    let sql = "SELECT * FROM t WHERE /* query.where AND */ TRUE ORDER BY /* query.order_by , */ id";
    let mut query = QueryComposer::<sqlx::Postgres>::new(sql);
    query
        .with_cursor(rank_cursor())
        .push_order_by(OrderByClause::parse("rank desc, id asc").unwrap());

    assert!(query.compose().is_ok());
}

/// A cursor whose `order_by` no longer matches the request's current
/// `order_by` (client changed their sort mid-pagination) is rejected.
#[test]
fn cursor_with_mismatched_order_by_is_rejected() {
    let sql = "SELECT * FROM t WHERE /* query.where AND */ TRUE ORDER BY /* query.order_by , */ id";
    let mut query = QueryComposer::<sqlx::Postgres>::new(sql);
    query
        .with_cursor(rank_cursor())
        .push_order_by(OrderByClause::parse("id asc").unwrap());

    assert!(matches!(
        query.compose().unwrap_err(),
        Error::CursorMismatch
    ));
}

/// A cursor and a plain filter both apply — pagination must not silently
/// drop a filter the caller also set.
#[test]
fn cursor_and_filter_are_combined_with_and() {
    let sql = "SELECT * FROM t WHERE /* query.where AND */ TRUE";
    let mut query = QueryComposer::<sqlx::Postgres>::new(sql);
    query
        .push_where(WhereClause::new("status = $1").bind_value("ACTIVE"))
        .with_cursor(rank_cursor());

    let statement = query.compose().unwrap();
    let (sql, values) = (statement.sql(), values(&statement));

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

/// The reason everything is numbered internally: a `?` binds by where it
/// sits in the text, so a slot *ahead* of the base query's own markers
/// binds ahead of them too. A value list built as base-then-fragments —
/// which is what this used to be — would hand `tenant_id` to the filter's
/// placeholder and the filter's value to `LIMIT`.
///
/// This is the layout the README documents and the one sqlc generates, so
/// it's asserted whole rather than by substring.
#[test]
fn non_positional_binds_in_textual_order_not_base_then_fragments() {
    let sql = "SELECT id FROM users WHERE /* query.where AND */ tenant_id = ? ORDER BY /* query.order_by , */ id LIMIT ?";
    let mut query = QueryComposer::<sqlx::Sqlite>::new(sql);
    query
        .bind_value("acme")
        .bind_value(50i64)
        .push_where(WhereClause::new("(rank) > ($1)").bind_value(10i64))
        .with_cursor(rank_cursor());

    let statement = query.compose().unwrap();
    let (sql, values) = (statement.sql(), values(&statement));

    assert_eq!(
        sql,
        "SELECT id FROM users WHERE ((rank) > (?)) AND ((rank < ?) OR (rank = ? AND id > ?)) AND tenant_id = ? ORDER BY rank DESC, id ASC , id LIMIT ?"
    );
    // Six placeholders, six values: the cursor's `rank` boundary is
    // referenced twice and so appears twice, which is the thing `$1` can
    // express and `?` cannot.
    assert_eq!(
        values,
        vec![
            Value::Int(10),
            Value::Int(42),
            Value::Int(42),
            Value::Int(7),
            Value::String("acme".into()),
            Value::Int(50),
        ]
    );
}

/// The same base query and the same clauses under PostgreSQL: numbered
/// throughout, each value once, and the value list in bind-declaration
/// order rather than textual order. Kept next to the SQLite case above so
/// the two are read together.
#[test]
fn positional_keeps_numbering_and_declaration_order() {
    let sql = "SELECT id FROM users WHERE /* query.where AND */ tenant_id = $1 ORDER BY /* query.order_by , */ id LIMIT $2";
    let mut query = QueryComposer::<sqlx::Postgres>::new(sql);
    query
        .bind_value("acme")
        .bind_value(50i64)
        .push_where(WhereClause::new("(rank) > ($1)").bind_value(10i64))
        .with_cursor(rank_cursor());

    let statement = query.compose().unwrap();
    let (sql, values) = (statement.sql(), values(&statement));

    assert_eq!(
        sql,
        "SELECT id FROM users WHERE ((rank) > ($3)) AND ((rank < $4) OR (rank = $4 AND id > $5)) AND tenant_id = $1 ORDER BY rank DESC, id ASC , id LIMIT $2"
    );
    assert_eq!(
        values,
        vec![
            Value::String("acme".into()),
            Value::Int(50),
            Value::Int(10),
            Value::Int(42),
            Value::Int(7),
        ]
    );
}

/// MySQL's `?` is SQLite's, but its lexer isn't: a `?` inside a
/// backtick-quoted identifier, a `#` comment or a backslash-escaped
/// string is text, not a placeholder. Getting this wrong turns a
/// character inside a literal into a bind parameter.
#[test]
fn mysql_markers_inside_quoting_and_comments_are_not_placeholders() {
    let sql = "SELECT `why?` FROM t # is ? a marker\nWHERE note = 'it\\'s ? here' AND /* query.where AND */ tenant = ?";
    let mut query = QueryComposer::<sqlx::MySql>::new(sql);
    query
        .bind_value("acme")
        .push_where(WhereClause::new("rank > $1").bind_value(10i64));

    let statement = query.compose().unwrap();
    let (sql, values) = (statement.sql(), values(&statement));

    assert!(sql.contains("SELECT `why?` FROM t"));
    assert!(sql.contains("'it\\'s ? here'"));
    assert!(sql.contains("AND rank > ? AND tenant = ?"));
    assert_eq!(values, vec![Value::Int(10), Value::String("acme".into())]);
}

/// `$1` in a base query for a `?` dialect is rejected rather than passed
/// through: SQLite would read it as a *named* parameter and never fill it
/// from a positional bind, and MySQL rejects it outright.
#[test]
fn numbered_placeholder_in_a_non_positional_base_is_rejected() {
    let sql = "SELECT * FROM t WHERE tenant = $1";
    let mut query = QueryComposer::<sqlx::Sqlite>::new(sql);
    query.bind_value("acme");

    assert!(matches!(
        query.compose(),
        Err(Error::UnsupportedPlaceholder { number: 1 })
    ));
}

/// Placeholders and values have to correspond one-to-one, because every
/// placeholder is resolved by index into the value list and every
/// fragment is shifted past the highest number ahead of it.
#[test]
fn a_base_query_with_more_placeholders_than_values_is_rejected() {
    let sql = "SELECT * FROM t WHERE tenant = $1 AND rank > $2";
    let mut query = QueryComposer::<sqlx::Postgres>::new(sql);
    query.bind_value("acme");

    assert!(matches!(
        query.compose(),
        Err(Error::BindMismatch {
            placeholders: 2,
            arguments: 1
        })
    ));
}

/// A `WHERE`-shaped value with nowhere to go is an error, not a silent
/// drop — the case that matters is a `Cursor` on a query whose author
/// never left a `/* query.where */` for it, which would otherwise return
/// page one forever.
#[test]
fn a_clause_with_no_slot_to_splice_it_into_is_rejected() {
    let sql = "SELECT * FROM t ORDER BY /* query.order_by , */ id";
    let mut query = QueryComposer::<sqlx::Postgres>::new(sql);
    query.with_cursor(rank_cursor());

    assert!(matches!(
        query.compose(),
        Err(Error::MissingSlot { name: "where" })
    ));
}

/// Multiple `.push_where()` calls accumulate (AND together) instead of the
/// last one replacing the others — e.g. an always-present tenant scope
/// plus a client-supplied filter.
#[test]
fn where_by_accumulates_across_multiple_calls() {
    let sql = "SELECT * FROM t WHERE /* query.where AND */ TRUE";
    let mut query = QueryComposer::<sqlx::Postgres>::new(sql);
    query
        .push_where(WhereClause::new("tenant_id = $1").bind_value("acme"))
        .push_where(WhereClause::new("status = $1").bind_value("ACTIVE"));

    let statement = query.compose().unwrap();
    let (sql, values) = (statement.sql(), values(&statement));

    assert!(sql.contains("WHERE (tenant_id = $1) AND (status = $2) AND TRUE"));
    assert_eq!(
        values,
        vec![Value::String("acme".into()), Value::String("ACTIVE".into())]
    );
}

/// Multiple `.push_order_by()` calls accumulate as tie-breakers in call
/// order, not the last one replacing the others.
#[test]
fn push_order_accumulates_as_tie_breakers_in_call_order() {
    let sql = "SELECT * FROM t ORDER BY /* query.order_by , */ id";
    let mut query = QueryComposer::<sqlx::Postgres>::new(sql);
    query
        .push_order_by(OrderByClause::parse("tenant_id asc").unwrap())
        .push_order_by(OrderByClause::parse("rank desc").unwrap());

    let sql = query.compose().unwrap().sql().to_owned();

    assert!(sql.contains("ORDER BY tenant_id ASC, rank DESC , id"));
}

/// When an explicit accumulated `order_by` matches the cursor's, the
/// cursor's `order_by` isn't appended as an extra tie-breaker on top of it
/// — the two are checked for equality, not concatenated.
#[test]
fn explicit_order_by_is_not_duplicated_by_a_matching_cursor() {
    // The cursor's `where` half needs somewhere to go: a clause with no
    // slot to splice it into is an error, not a silent drop.
    let sql = "SELECT * FROM t WHERE /* query.where AND */ TRUE ORDER BY /* query.order_by , */ id";
    let mut query = QueryComposer::<sqlx::Postgres>::new(sql);
    query
        .push_order_by(OrderByClause::parse("rank desc, id asc").unwrap())
        .with_cursor(rank_cursor());

    let sql = query.compose().unwrap().sql().to_owned();

    assert!(sql.contains("ORDER BY rank DESC, id ASC , id"));
    assert_eq!(sql.matches("rank DESC").count(), 1);
}

/// `Value::Bytes` completes SQLite's storage classes, so a `BLOB`
/// parameter no longer has to be smuggled through as text.
#[test]
fn bytes_bind_as_a_value() {
    let sql = "SELECT * FROM t WHERE /* query.where AND */ receipt = ?";
    let mut query = QueryComposer::<sqlx::Sqlite>::new(sql);
    query
        .bind_value(vec![0xDE_u8, 0xAD, 0xBE, 0xEF])
        .push_where(WhereClause::new("rank > $1").bind_value(10i64));

    let statement = query.compose().unwrap();
    let (sql, values) = (statement.sql(), values(&statement));

    assert_eq!(sql, "SELECT * FROM t WHERE rank > ? AND receipt = ?");
    assert_eq!(
        values,
        vec![Value::Int(10), Value::Bytes(vec![0xDE, 0xAD, 0xBE, 0xEF])]
    );
}

/// `bind` takes anything sqlx can encode, including types `Value` has no
/// variant for and no `From` impl reaching — a `NaiveDate` here, a
/// `uuid::Uuid` or `serde_json::Value` in a real schema. The composer
/// never sees the value, so `QueryArgument::value` answers `None` and
/// `compose()` can't print it back.
#[test]
fn bind_accepts_a_type_value_cannot_hold() {
    let sql = "SELECT * FROM t WHERE /* query.where AND */ due = ?";
    let mut query = QueryComposer::<sqlx::Sqlite>::new(sql);
    query
        .bind(chrono::NaiveDate::from_ymd_opt(2026, 9, 22).unwrap())
        .push_where(WhereClause::new("rank > $1").bind_value(10i64));

    let statement = query.compose().unwrap();

    assert_eq!(
        statement.sql(),
        "SELECT * FROM t WHERE rank > ? AND due = ?"
    );
    // Textual order: the slot precedes the base query's own `?`.
    assert_eq!(statement.arguments()[0].value(), Some(&Value::Int(10)));
    assert_eq!(statement.arguments()[1].value(), None);
}
