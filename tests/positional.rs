//! How each driver's placeholders survive a rewrite.
//!
//! Two families, and the split is not the one the syntax suggests:
//!
//! - PostgreSQL's `$N` and SQLite's `?N` *name* the value they want. A
//!   fragment spliced ahead of one does not disturb it, and one value can be
//!   named twice.
//! - MySQL's `?` does not, and cannot: `?1` is a syntax error there and `$1`
//!   parses as a column name. It takes a value per placeholder in the order
//!   they appear, so the values are replayed to match.
//!
//! These assert the SQL. Whether the values actually land where they should is
//! `sqlite.rs` and `mysql.rs`, which need a database to answer.

#![cfg(any(feature = "sqlite", feature = "mysql", feature = "postgres"))]

use sqlx_query::{Dialect, Error, QueryWriter};

fn rewrite<DB: Dialect>(
    sql: &str,
    apply: impl FnOnce(&mut QueryWriter<'_, DB>),
) -> Result<String, Error> {
    let mut writer = QueryWriter::<DB>::new(sql).unwrap();
    apply(&mut writer);
    writer.sql()
}

/// The drivers whose placeholders carry a number. Same base query, same
/// fragment, same structure out -- only the sigil differs.
mod numbered {
    #[allow(unused_imports)]
    use super::{Error, rewrite};

    #[cfg(feature = "postgres")]
    #[test]
    fn postgres_renumbers_the_fragment_and_leaves_the_base_alone() {
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

    #[cfg(feature = "sqlite")]
    #[test]
    fn sqlite_does_the_same_with_question_marks() {
        let sql =
            rewrite::<sqlx::Sqlite>("SELECT id FROM users WHERE tenant_id = ? LIMIT ?", |w| {
                w.filter_by("role = ?");
            })
            .unwrap();

        assert_eq!(
            sql,
            "SELECT id FROM users WHERE tenant_id = ?1 AND role = ?3 LIMIT ?2"
        );
    }

    /// Bare `?` on the way in, numbered on the way out. The base query is
    /// written the way anyone would write it; the numbering is this crate's.
    #[cfg(feature = "sqlite")]
    #[test]
    fn sqlite_numbers_placeholders_that_arrived_bare() {
        let sql = rewrite::<sqlx::Sqlite>("SELECT id FROM users WHERE tenant_id = ?", |w| {
            w.filter_by("role = ?");
        })
        .unwrap();

        assert_eq!(
            sql,
            "SELECT id FROM users WHERE tenant_id = ?1 AND role = ?2"
        );
    }

    #[cfg(feature = "postgres")]
    #[test]
    fn postgres_allows_one_value_in_two_places() {
        let sql = rewrite::<sqlx::Postgres>("SELECT id FROM users", |w| {
            w.filter_by("a = $1 OR b = $1");
        })
        .unwrap();

        assert_eq!(sql, "SELECT id FROM users WHERE a = $1 OR b = $1");
    }

    /// And so does SQLite, now that it is numbered. It could not when the
    /// output was a bare `?`.
    #[cfg(feature = "sqlite")]
    #[test]
    fn sqlite_allows_one_value_in_two_places() {
        let sql = rewrite::<sqlx::Sqlite>("SELECT id FROM users", |w| {
            w.filter_by("a = ?1 OR b = ?1");
        })
        .unwrap();

        assert_eq!(sql, "SELECT id FROM users WHERE a = ?1 OR b = ?1");
    }

    #[cfg(feature = "sqlite")]
    #[test]
    fn ordering_may_move_a_limit_placeholder() {
        let sql = rewrite::<sqlx::Sqlite>("SELECT id FROM users LIMIT ?", |w| {
            w.order_by("? asc");
        })
        .unwrap();

        assert_eq!(sql, "SELECT id FROM users ORDER BY ?2 ASC LIMIT ?1");
    }

    /// Replacing the limit deletes the placeholder that was in it, so the
    /// value bound for it would have nowhere to go. Numbering cannot help
    /// with a placeholder that is simply gone.
    #[cfg(feature = "sqlite")]
    #[test]
    fn replacing_a_limit_that_held_a_placeholder_is_refused() {
        let error = rewrite::<sqlx::Sqlite>("SELECT id FROM users WHERE t = ? LIMIT ?", |w| {
            w.filter_by("role = ?").limit(50);
        })
        .unwrap_err();

        assert!(matches!(error, Error::Orphaned), "{error:?}");
    }

    #[cfg(feature = "sqlite")]
    #[test]
    fn replacing_a_literal_limit_is_fine() {
        let sql = rewrite::<sqlx::Sqlite>("SELECT id FROM users WHERE t = ? LIMIT 10", |w| {
            w.filter_by("role = ?").limit(50);
        })
        .unwrap();

        assert_eq!(
            sql,
            "SELECT id FROM users WHERE t = ?1 AND role = ?2 LIMIT 50"
        );
    }

    #[cfg(feature = "sqlite")]
    #[test]
    fn a_question_mark_in_a_string_literal_is_not_a_placeholder() {
        let sql = rewrite::<sqlx::Sqlite>("SELECT id FROM users WHERE note = '? ?'", |w| {
            w.filter_by("role = ?");
        })
        .unwrap();

        assert_eq!(sql, "SELECT id FROM users WHERE note = '? ?' AND role = ?1");
    }
}

/// MySQL, which cannot number and so has to be replayed.
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

    /// The placeholders stay bare and in place; what moves is the order the
    /// values are sent in, which this cannot show. See `mysql.rs`.
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

    #[test]
    fn a_question_mark_in_a_string_literal_is_not_a_placeholder() {
        let sql = rewrite::<MySql>("SELECT id FROM users WHERE note = '? ?'", |w| {
            w.filter_by("role = ?");
        })
        .unwrap();

        assert_eq!(sql, "SELECT id FROM users WHERE note = '? ?' AND role = ?");
    }

    /// The one thing bare `?` cannot express. PostgreSQL and SQLite both
    /// allow one value in two places; MySQL has no way to name an earlier one,
    /// so a fragment that asks for it is refused rather than silently given
    /// two values.
    #[test]
    fn one_value_wanted_by_two_placeholders_is_refused() {
        let error = rewrite::<MySql>("SELECT id FROM users", |w| {
            w.filter_by("a = ?1 OR b = ?1");
        })
        .unwrap_err();

        assert!(matches!(error, Error::Positional), "{error:?}");
    }
}
