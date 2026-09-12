//! `sql!` scans the skeleton at compile time.
#![cfg(all(feature = "macros", feature = "postgres"))]

use sqlx::Postgres;
use sqlx_query::{QueryFragment, QueryMapping, QueryTemplate, Value, sql};

/// The point of the macro: a skeleton in a `static`, with no run-time parse
/// and no `unwrap` for a failure that cannot happen.
static VOLUMES: QueryTemplate<Postgres> = sql!(
    "SELECT id, title FROM volumes \
     WHERE tenant_id = $1 \
     /* AND query.filter */ \
     /* ORDER BY query.order */"
);

#[test]
fn a_static_template_declares_its_slots() {
    assert_eq!(VOLUMES.slots().collect::<Vec<_>>(), ["filter", "order"]);
}

#[test]
fn the_skeleton_is_the_statement_without_its_sentinels() {
    assert_eq!(
        VOLUMES.skeleton(),
        "SELECT id, title FROM volumes WHERE tenant_id = $1  "
    );
}

#[test]
fn a_compiled_template_splices_like_a_parsed_one() {
    let mut predicate = QueryFragment::<Postgres, Value>::new();
    predicate.push("read_count > ").push_bind(Value::Int(100));

    let mut order = QueryFragment::<Postgres, Value>::new();
    order.push("\"title\" DESC");

    let sql = VOLUMES
        .builder(&QueryMapping::new())
        .bind(7_i64)
        .fill("filter", &predicate)
        .fill("order", &order)
        .sql();

    assert_eq!(
        sql,
        "SELECT id, title FROM volumes WHERE tenant_id = $1 \
         AND read_count > $2 ORDER BY \"title\" DESC"
    );
}

/// The macro and the runtime parser are the same scanner, so they must agree.
#[test]
fn the_macro_and_parse_produce_the_same_template() {
    const SOURCE: &str = "SELECT id, title FROM volumes \
                          WHERE tenant_id = $1 \
                          /* AND query.filter */ \
                          /* ORDER BY query.order */";

    let parsed = QueryTemplate::<Postgres>::parse(SOURCE).unwrap();

    assert_eq!(parsed.skeleton(), VOLUMES.skeleton());
    assert_eq!(
        parsed.slots().collect::<Vec<_>>(),
        VOLUMES.slots().collect::<Vec<_>>()
    );
}
