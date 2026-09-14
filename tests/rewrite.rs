//! What the rewrite produces, and what it refuses to produce.
//!
//! PostgreSQL throughout: `$N` is not positional, so these are about the tree
//! surgery alone. The dialects where placement also decides binding order are
//! in `positional.rs`.

#![cfg(feature = "postgres")]

use sqlx::Postgres;
use sqlx_query::{Error, QueryWriter};

/// Rewrites `sql` and returns what came out.
fn rewrite(sql: &str, apply: impl FnOnce(&mut QueryWriter<Postgres>)) -> Result<String, Error> {
    let mut writer = QueryWriter::<Postgres>::new(sql)?;
    apply(&mut writer);
    writer.sql()
}

#[test]
fn filter_becomes_the_where_when_there_is_none() {
    let sql = rewrite("SELECT id FROM users", |w| {
        w.filter_by("role = 'admin'");
    })
    .unwrap();

    assert_eq!(sql, "SELECT id FROM users WHERE role = 'admin'");
}

#[test]
fn filter_joins_an_existing_where() {
    let sql = rewrite("SELECT id FROM users WHERE tenant_id = $1", |w| {
        w.filter_by("role = 'admin'");
    })
    .unwrap();

    assert_eq!(
        sql,
        "SELECT id FROM users WHERE tenant_id = $1 AND role = 'admin'"
    );
}

#[test]
fn filters_accumulate() {
    let sql = rewrite("SELECT id FROM users", |w| {
        w.filter_by("role = 'admin'")
            .filter_by("active")
            .filter_by("age > 18");
    })
    .unwrap();

    assert_eq!(
        sql,
        "SELECT id FROM users WHERE role = 'admin' AND active AND age > 18"
    );
}

/// The case a text splice gets wrong. `AND` binds tighter than `OR`, so
/// appending ` AND role = 'admin'` to this `WHERE` would quietly re-associate
/// it into `a = 1 OR (b = 2 AND role = 'admin')`.
#[test]
fn an_or_in_the_base_query_is_parenthesised() {
    let sql = rewrite("SELECT id FROM users WHERE a = 1 OR b = 2", |w| {
        w.filter_by("role = 'admin'");
    })
    .unwrap();

    assert_eq!(
        sql,
        "SELECT id FROM users WHERE (a = 1 OR b = 2) AND role = 'admin'"
    );
}

/// And the same hazard from the other side.
#[test]
fn an_or_in_the_fragment_is_parenthesised() {
    let sql = rewrite("SELECT id FROM users WHERE tenant_id = $1", |w| {
        w.filter_by("role = 'admin' OR role = 'owner'");
    })
    .unwrap();

    assert_eq!(
        sql,
        "SELECT id FROM users WHERE tenant_id = $1 AND (role = 'admin' OR role = 'owner')"
    );
}

#[test]
fn an_and_is_left_alone() {
    let sql = rewrite("SELECT id FROM users WHERE a = 1 AND b = 2", |w| {
        w.filter_by("c = 3");
    })
    .unwrap();

    assert_eq!(sql, "SELECT id FROM users WHERE a = 1 AND b = 2 AND c = 3");
}

#[test]
fn order_by_goes_in_front_and_the_base_becomes_a_tiebreaker() {
    let sql = rewrite("SELECT id FROM users ORDER BY id", |w| {
        w.order_by("name desc");
    })
    .unwrap();

    assert_eq!(sql, "SELECT id FROM users ORDER BY name DESC, id");
}

#[test]
fn order_by_does_not_repeat_a_column_the_base_already_named() {
    let sql = rewrite("SELECT id FROM users ORDER BY id", |w| {
        w.order_by("id asc");
    })
    .unwrap();

    assert_eq!(sql, "SELECT id FROM users ORDER BY id ASC");
}

/// Repeated calls append, in the order they were made.
#[test]
fn order_by_accumulates() {
    let sql = rewrite("SELECT id FROM users ORDER BY id", |w| {
        w.order_by("name desc").order_by("created_at asc");
    })
    .unwrap();

    assert_eq!(
        sql,
        "SELECT id FROM users ORDER BY name DESC, created_at ASC, id"
    );
}

/// First mention of a column wins, and settles both its position and its
/// direction -- whether the second mention came from another call or from the
/// base query. Ordering by a column twice is not an error; the second one just
/// has nothing left to say.
#[test]
fn order_by_does_not_repeat_a_column_an_earlier_call_named() {
    let sql = rewrite("SELECT id FROM users ORDER BY id", |w| {
        w.order_by("name asc").order_by("name desc");
    })
    .unwrap();

    assert_eq!(sql, "SELECT id FROM users ORDER BY name ASC, id");
}

/// `limit` replaces rather than accumulating: there is only one `LIMIT`, and
/// two calls cannot both be honoured.
#[test]
fn limit_keeps_the_last_call() {
    let sql = rewrite("SELECT id FROM users", |w| {
        w.limit(10).limit(20);
    })
    .unwrap();

    assert_eq!(sql, "SELECT id FROM users LIMIT 20");
}

#[test]
fn order_by_takes_a_list() {
    let sql = rewrite("SELECT id FROM users", |w| {
        w.order_by("name desc, created_at asc");
    })
    .unwrap();

    assert_eq!(
        sql,
        "SELECT id FROM users ORDER BY name DESC, created_at ASC"
    );
}

/// Orphaning is not a `?` problem -- PostgreSQL would be handed a value for a
/// `$2` that is no longer in the statement.
#[test]
fn replacing_a_limit_that_held_a_placeholder_is_refused() {
    let error = rewrite("SELECT id FROM users WHERE tenant_id = $1 LIMIT $2", |w| {
        w.limit(50);
    })
    .unwrap_err();

    assert!(matches!(error, Error::Orphaned), "{error:?}");
}

#[test]
fn limit_replaces_the_base_limit() {
    let sql = rewrite("SELECT id FROM users LIMIT 10", |w| {
        w.limit(50);
    })
    .unwrap();

    assert_eq!(sql, "SELECT id FROM users LIMIT 50");
}

// -- placeholders ---------------------------------------------------------

#[test]
fn a_fragment_numbers_from_one_and_is_renumbered_to_follow() {
    let sql = rewrite("SELECT id FROM users WHERE tenant_id = $1", |w| {
        w.filter_by("role = $1");
    })
    .unwrap();

    assert_eq!(
        sql,
        "SELECT id FROM users WHERE tenant_id = $1 AND role = $2"
    );
}

/// The base query keeps its own numbering even though the fragment now renders
/// between its two placeholders -- `$N` names a value, not a position.
#[test]
fn a_base_placeholder_after_the_where_keeps_its_number() {
    let sql = rewrite("SELECT id FROM users WHERE tenant_id = $1 LIMIT $2", |w| {
        w.filter_by("role = $1");
    })
    .unwrap();

    assert_eq!(
        sql,
        "SELECT id FROM users WHERE tenant_id = $1 AND role = $3 LIMIT $2"
    );
}

#[test]
fn placeholders_from_several_fragments_are_numbered_in_order() {
    let sql = rewrite("SELECT id FROM users WHERE tenant_id = $1", |w| {
        w.filter_by("role = $1").filter_by("age > $1");
    })
    .unwrap();

    assert_eq!(
        sql,
        "SELECT id FROM users WHERE tenant_id = $1 AND role = $2 AND age > $3"
    );
}

/// Reusing `$1` binds the same value twice, which is what PostgreSQL means by
/// it -- so the fragment after it claims `$2`, not `$3`.
#[test]
fn a_repeated_placeholder_claims_one_value() {
    let sql = rewrite("SELECT id FROM users WHERE a = $1 OR b = $1", |w| {
        w.filter_by("role = $1");
    })
    .unwrap();

    assert_eq!(
        sql,
        "SELECT id FROM users WHERE (a = $1 OR b = $1) AND role = $2"
    );
}

/// The reason this is a tree walk. A renumberer that scanned text would find
/// the `$1` inside the string and rewrite it; this one never sees it, because
/// it is a literal and not a placeholder node.
#[test]
fn a_placeholder_inside_a_string_literal_is_not_a_placeholder() {
    let sql = rewrite("SELECT id FROM users WHERE note = '$1 of $2'", |w| {
        w.filter_by("role = $1");
    })
    .unwrap();

    assert_eq!(
        sql,
        "SELECT id FROM users WHERE note = '$1 of $2' AND role = $1"
    );
}

// -- what it refuses ------------------------------------------------------

/// The check that makes a fragment safe to accept as text: the expression
/// parses, and then there is a statement left over.
#[test]
fn a_fragment_with_a_statement_after_it_is_refused() {
    let error = rewrite("SELECT id FROM users", |w| {
        w.filter_by("role = 'admin'; DROP TABLE users");
    })
    .unwrap_err();

    assert!(matches!(error, Error::Trailing { .. }), "{error:?}");
}

#[test]
fn a_trailing_fragment_in_order_by_is_refused() {
    let error = rewrite("SELECT id FROM users", |w| {
        w.order_by("name asc; DROP TABLE users");
    })
    .unwrap_err();

    assert!(matches!(error, Error::Trailing { .. }), "{error:?}");
}

#[test]
fn a_fragment_that_is_not_an_expression_is_refused() {
    let error = rewrite("SELECT id FROM users", |w| {
        w.filter_by("= = =");
    })
    .unwrap_err();

    assert!(matches!(error, Error::Fragment { .. }), "{error:?}");
}

/// sqlparser will read a bare keyword as an identifier, so `FROM WHERE` parses
/// as the expression `FROM` with `WHERE` left over. Insisting the parser
/// reached the end is what turns that leniency back into a rejection.
#[test]
fn a_fragment_of_bare_keywords_does_not_slip_through_as_an_identifier() {
    let error = rewrite("SELECT id FROM users", |w| {
        w.filter_by("FROM WHERE");
    })
    .unwrap_err();

    assert!(matches!(error, Error::Trailing { .. }), "{error:?}");
}

#[test]
fn a_union_has_no_single_select_to_filter() {
    let error = rewrite("SELECT id FROM a UNION SELECT id FROM b", |w| {
        w.filter_by("role = 'admin'");
    })
    .unwrap_err();

    assert!(matches!(error, Error::SetOperation), "{error:?}");
}

/// Ordering a union is unambiguous -- it applies to the whole result -- so it
/// is allowed even though filtering one is not.
#[test]
fn a_union_can_still_be_ordered() {
    let sql = rewrite("SELECT id FROM a UNION SELECT id FROM b", |w| {
        w.order_by("id desc");
    })
    .unwrap();

    assert_eq!(
        sql,
        "SELECT id FROM a UNION SELECT id FROM b ORDER BY id DESC"
    );
}

#[test]
fn a_group_by_makes_a_filter_ambiguous() {
    let error = rewrite("SELECT role, count(*) FROM users GROUP BY role", |w| {
        w.filter_by("count(*) > 5");
    })
    .unwrap_err();

    assert!(matches!(error, Error::Grouped), "{error:?}");
}

#[test]
fn a_statement_that_is_not_a_query_is_refused() {
    let error = QueryWriter::<Postgres>::new("INSERT INTO users (id) VALUES (1)").unwrap_err();

    assert!(matches!(error, Error::NotQuery), "{error:?}");
}

#[test]
fn two_statements_are_refused() {
    let error = QueryWriter::<Postgres>::new("SELECT 1; SELECT 2").unwrap_err();

    assert!(matches!(error, Error::NotQuery), "{error:?}");
}

#[test]
fn a_trailing_semicolon_is_not_a_second_statement() {
    let sql = rewrite("SELECT id FROM users;", |w| {
        w.filter_by("active");
    })
    .unwrap();

    assert_eq!(sql, "SELECT id FROM users WHERE active");
}

#[test]
fn a_query_that_does_not_parse_is_refused() {
    let error = QueryWriter::<Postgres>::new("SELECT * FROM").unwrap_err();

    assert!(matches!(error, Error::Query(_)), "{error:?}");
}

/// A failure recorded mid-chain has to keep being reported, or the second call
/// would look like it succeeded.
#[test]
fn a_failure_is_reported_every_time_it_is_asked_for() {
    let mut writer = QueryWriter::<Postgres>::new("SELECT id FROM users").unwrap();
    writer.filter_by("role = 'admin'; DROP TABLE users");

    assert!(matches!(writer.sql(), Err(Error::Trailing { .. })));
    assert!(matches!(writer.sql(), Err(Error::Trailing { .. })));
}

/// And the first one is the one that explains the rest.
#[test]
fn the_first_failure_wins() {
    let mut writer = QueryWriter::<Postgres>::new("SELECT id FROM users").unwrap();
    writer
        .filter_by("= = =")
        .filter_by("role = 'admin'; DROP TABLE users");

    assert!(matches!(writer.sql(), Err(Error::Fragment { .. })));
}
