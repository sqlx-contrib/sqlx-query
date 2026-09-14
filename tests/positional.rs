//! Where a rewrite is safe for `?` and where it is not.
//!
//! MySQL and SQLite bind `?` by its position in the text, so moving one moves
//! what it binds. A rewrite that would do that is refused, because the query
//! it produces runs and returns the wrong rows -- the worst kind of wrong.

#![cfg(any(feature = "sqlite", feature = "mysql"))]

use sqlx_query::{Dialect, Error, QueryWriter};

fn rewrite<DB: Dialect>(
    sql: &str,
    apply: impl FnOnce(&mut QueryWriter<DB>),
) -> Result<String, Error> {
    let mut writer = QueryWriter::<DB>::new(sql).unwrap();
    apply(&mut writer);
    writer.sql()
}

#[cfg(feature = "sqlite")]
mod sqlite {
    use super::{Error, rewrite};
    use sqlx::Sqlite;

    /// Nothing follows the `WHERE`, so the fragment's placeholder lands last
    /// and every existing one keeps its position.
    #[test]
    fn a_fragment_after_the_last_placeholder_is_fine() {
        let sql = rewrite::<Sqlite>("SELECT id FROM users WHERE tenant_id = ?", |w| {
            w.filter_by("role = ?");
        })
        .unwrap();

        assert_eq!(sql, "SELECT id FROM users WHERE tenant_id = ? AND role = ?");
    }

    /// Here it is not. The `LIMIT ?` was the second placeholder and the
    /// fragment makes it the third, so it would bind the page size to the
    /// role.
    #[test]
    fn a_fragment_before_an_existing_placeholder_is_refused() {
        let error = rewrite::<Sqlite>("SELECT id FROM users WHERE tenant_id = ? LIMIT ?", |w| {
            w.filter_by("role = ?");
        })
        .unwrap_err();

        assert!(matches!(error, Error::Positional), "{error:?}");
    }

    /// A fragment with no placeholders of its own moves nothing, so the same
    /// query rewrites cleanly.
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

    /// Ordering renders after the `WHERE`, so an ordering placeholder is
    /// still ahead of a `LIMIT`.
    #[test]
    fn ordering_can_move_a_limit_placeholder_too() {
        let error = rewrite::<Sqlite>("SELECT id FROM users LIMIT ?", |w| {
            w.order_by("? asc");
        })
        .unwrap_err();

        assert!(matches!(error, Error::Positional), "{error:?}");
    }

    /// Replacing the limit deletes the placeholder that was in it, so the
    /// value bound for it would have nowhere to go. That is a different
    /// complaint from reordering, and gets its own.
    #[test]
    fn replacing_a_limit_that_held_a_placeholder_is_refused() {
        let error = rewrite::<Sqlite>("SELECT id FROM users WHERE tenant_id = ? LIMIT ?", |w| {
            w.filter_by("role = ?").limit(50);
        })
        .unwrap_err();

        assert!(matches!(error, Error::Orphaned), "{error:?}");
    }

    /// With no placeholder in the base limit there is nothing to orphan.
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
}

#[cfg(feature = "mysql")]
mod mysql {
    use super::{Error, rewrite};
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
    fn a_fragment_before_an_existing_placeholder_is_refused() {
        let error = rewrite::<MySql>("SELECT id FROM users WHERE tenant_id = ? LIMIT ?", |w| {
            w.filter_by("role = ?");
        })
        .unwrap_err();

        assert!(matches!(error, Error::Positional), "{error:?}");
    }
}

/// The same rewrite PostgreSQL is asked for above, to show the refusal is
/// about `?` and not about the rewrite.
#[cfg(feature = "postgres")]
#[test]
fn postgres_renumbers_instead_of_refusing() {
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
