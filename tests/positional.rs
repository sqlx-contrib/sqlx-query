//! Rewrites under a driver that binds `?` by position.
//!
//! MySQL and SQLite take a value per placeholder, in the order the
//! placeholders appear. A fragment spliced into the middle of a query shifts
//! every one after it -- so the values are replayed in the order the finished
//! statement asks for, rather than the order they were given. These are the
//! cases where those two orders differ.

#![cfg(any(feature = "sqlite", feature = "mysql"))]

use sqlx_query::{Dialect, Error, QueryWriter};

fn rewrite<DB: Dialect>(
    sql: &str,
    apply: impl FnOnce(&mut QueryWriter<'_, DB>),
) -> Result<String, Error> {
    let mut writer = QueryWriter::<DB>::new(sql).unwrap();
    apply(&mut writer);
    writer.sql()
}

#[cfg(feature = "sqlite")]
mod sqlite {
    use super::{Error, rewrite};
    use sqlx::Sqlite;

    #[test]
    fn a_fragment_after_the_last_placeholder_is_fine() {
        let sql = rewrite::<Sqlite>("SELECT id FROM users WHERE tenant_id = ?", |w| {
            w.filter_by("role = ?");
        })
        .unwrap();

        assert_eq!(sql, "SELECT id FROM users WHERE tenant_id = ? AND role = ?");
    }

    /// The fragment renders ahead of the base query's `LIMIT ?`, so the values
    /// are no longer wanted in the order they were given. That is a
    /// reordering, not a refusal -- see `sqlite.rs` for the same rewrite run
    /// against a database.
    #[test]
    fn a_fragment_before_an_existing_placeholder_is_rewritten() {
        let sql = rewrite::<Sqlite>("SELECT id FROM users WHERE tenant_id = ? LIMIT ?", |w| {
            w.filter_by("role = ?");
        })
        .unwrap();

        assert_eq!(
            sql,
            "SELECT id FROM users WHERE tenant_id = ? AND role = ? LIMIT ?"
        );
    }

    #[test]
    fn a_fragment_without_placeholders_moves_nothing() {
        let sql = rewrite::<Sqlite>("SELECT id FROM users WHERE tenant_id = ? LIMIT ?", |w| {
            w.filter_by("role = 'admin'");
        })
        .unwrap();

        assert_eq!(
            sql,
            "SELECT id FROM users WHERE tenant_id = ? AND role = 'admin' LIMIT ?"
        );
    }

    /// Ordering renders after the `WHERE` and before the `LIMIT`, so it moves
    /// a placeholder just as a filter does.
    #[test]
    fn ordering_can_move_a_limit_placeholder_too() {
        let sql = rewrite::<Sqlite>("SELECT id FROM users LIMIT ?", |w| {
            w.order_by("? asc");
        })
        .unwrap();

        assert_eq!(sql, "SELECT id FROM users ORDER BY ? ASC LIMIT ?");
    }

    /// Replacing the limit deletes the placeholder that was in it, so the
    /// value bound for it would have nowhere to go. Reordering can be handled;
    /// a value with no placeholder at all cannot.
    #[test]
    fn replacing_a_limit_that_held_a_placeholder_is_refused() {
        let error = rewrite::<Sqlite>("SELECT id FROM users WHERE tenant_id = ? LIMIT ?", |w| {
            w.filter_by("role = ?").limit(50);
        })
        .unwrap_err();

        assert!(matches!(error, Error::Orphaned), "{error:?}");
    }

    #[test]
    fn replacing_a_literal_limit_is_fine() {
        let sql = rewrite::<Sqlite>("SELECT id FROM users WHERE tenant_id = ? LIMIT 10", |w| {
            w.filter_by("role = ?").limit(50);
        })
        .unwrap();

        assert_eq!(
            sql,
            "SELECT id FROM users WHERE tenant_id = ? AND role = ? LIMIT 50"
        );
    }

    #[test]
    fn a_question_mark_in_a_string_literal_is_not_a_placeholder() {
        let sql = rewrite::<Sqlite>("SELECT id FROM users WHERE note = '? ?'", |w| {
            w.filter_by("role = ?");
        })
        .unwrap();

        assert_eq!(sql, "SELECT id FROM users WHERE note = '? ?' AND role = ?");
    }

    /// `$1` twice binds one value twice, which `?` has no way to say. The
    /// reordering machinery cannot help here: there is no text to render.
    #[test]
    fn one_value_wanted_by_two_placeholders_is_refused() {
        let error = rewrite::<Sqlite>("SELECT id FROM users", |w| {
            w.filter_by("a = $1 OR b = $1");
        })
        .unwrap_err();

        assert!(matches!(error, Error::Positional), "{error:?}");
    }
}

#[cfg(feature = "mysql")]
mod mysql {
    use super::rewrite;
    use sqlx::MySql;

    #[test]
    fn a_fragment_after_the_last_placeholder_is_fine() {
        let sql = rewrite::<MySql>("SELECT id FROM users WHERE tenant_id = ?", |w| {
            w.filter_by("role = ?");
        })
        .unwrap();

        assert_eq!(sql, "SELECT id FROM users WHERE tenant_id = ? AND role = ?");
    }

    #[test]
    fn a_fragment_before_an_existing_placeholder_is_rewritten() {
        let sql = rewrite::<MySql>("SELECT id FROM users WHERE tenant_id = ? LIMIT ?", |w| {
            w.filter_by("role = ?");
        })
        .unwrap();

        assert_eq!(
            sql,
            "SELECT id FROM users WHERE tenant_id = ? AND role = ? LIMIT ?"
        );
    }
}

/// PostgreSQL reaches the same place by a different route: `$N` names the
/// value it wants, so nothing has to be replayed in a different order.
#[cfg(feature = "postgres")]
#[test]
fn postgres_renumbers_rather_than_reordering() {
    let sql =
        rewrite::<sqlx::Postgres>("SELECT id FROM users WHERE tenant_id = $1 LIMIT $2", |w| {
            w.filter_by("role = $1");
        })
        .unwrap();

    assert_eq!(
        sql,
        "SELECT id FROM users WHERE tenant_id = $1 AND role = $3 LIMIT $2"
    );
}

/// And `$1` twice is ordinary there.
#[cfg(feature = "postgres")]
#[test]
fn postgres_allows_one_value_in_two_places() {
    let sql = rewrite::<sqlx::Postgres>("SELECT id FROM users", |w| {
        w.filter_by("a = $1 OR b = $1");
    })
    .unwrap();

    assert_eq!(sql, "SELECT id FROM users WHERE a = $1 OR b = $1");
}
