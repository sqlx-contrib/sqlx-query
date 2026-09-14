//! The rewrite, driver by driver.
//!
//! One file, mirroring `src/writer.rs`, and one module per driver, because the
//! driver is what the answers actually depend on: the same base query comes out
//! with `$N` under PostgreSQL and `?N` under SQLite.
//!
//! Within each, `placeholders` compares SQL, which is all a numbering question
//! needs, and `database` runs it, which is the only way to tell whether a value
//! landed on the placeholder it was meant for.

#[cfg(feature = "postgres")]
mod postgres {
    use sqlx::Postgres;
    use sqlx_query::{Error, QueryWriter};

    /// Rewrites `sql` and returns what came out.
    fn rewrite(sql: &str, apply: impl FnOnce(&mut QueryWriter<Postgres>)) -> Result<String, Error> {
        let mut writer = QueryWriter::<Postgres>::new(sql)?;
        apply(&mut writer);
        writer.sql()
    }

    /// What the rewrite does to the tree. None of these are about PostgreSQL
    /// in particular -- it is the vehicle, because its `$N` keeps the SQL
    /// easiest to read.
    mod tree {
        use super::rewrite;
        use sqlx::Postgres;
        use sqlx_query::{Error, QueryWriter};

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

        #[test]
        fn order_by_does_not_repeat_a_column_the_base_already_named() {
            let sql = rewrite("SELECT id FROM users ORDER BY id", |w| {
                w.order_by("id asc");
            })
            .unwrap();

            assert_eq!(sql, "SELECT id FROM users ORDER BY id ASC");
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

        #[test]
        fn limit_replaces_the_base_limit() {
            let sql = rewrite("SELECT id FROM users LIMIT 10", |w| {
                w.limit(50);
            })
            .unwrap();

            assert_eq!(sql, "SELECT id FROM users LIMIT 50");
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
            let error =
                QueryWriter::<Postgres>::new("INSERT INTO users (id) VALUES (1)").unwrap_err();

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
    }

    /// `$N` names the value it wants, so a fragment spliced into the middle
    /// of the query renumbers only itself.
    mod placeholders {
        use super::rewrite;
        use sqlx::Postgres;
        use sqlx_query::{Error, QueryWriter};

        /// Counting is checked on the way to the driver, so a forgotten
        /// fragment value is named here rather than surfacing from the driver
        /// in its own terms.
        #[test]
        fn binding_too_few_values_is_refused() {
            let mut writer =
                QueryWriter::<Postgres>::new("SELECT id FROM t WHERE a = $1 AND b = $2").unwrap();
            writer.bind(1_i64);

            let Err(error) = writer.build() else {
                panic!("expected an error")
            };
            assert!(
                matches!(
                    error,
                    Error::Arity {
                        wanted: 2,
                        given: 1
                    }
                ),
                "{error:?}"
            );
        }

        #[test]
        fn binding_too_many_values_is_refused() {
            let mut writer = QueryWriter::<Postgres>::new("SELECT id FROM t WHERE a = $1").unwrap();
            writer.bind(1_i64).bind(2_i64);

            let Err(error) = writer.build() else {
                panic!("expected an error")
            };
            assert!(
                matches!(
                    error,
                    Error::Arity {
                        wanted: 1,
                        given: 2
                    }
                ),
                "{error:?}"
            );
        }

        /// A fragment's values are counted too, so forgetting them is caught.
        #[test]
        fn forgetting_a_fragments_value_is_refused() {
            let mut writer = QueryWriter::<Postgres>::new("SELECT id FROM t WHERE a = $1").unwrap();
            writer.bind(1_i64).filter_by("b = $1");

            let Err(error) = writer.build() else {
                panic!("expected an error")
            };
            assert!(
                matches!(
                    error,
                    Error::Arity {
                        wanted: 2,
                        given: 1
                    }
                ),
                "{error:?}"
            );
        }

        /// `sql()` renders without the check, so a query can be inspected or
        /// logged before anything is bound.
        #[test]
        fn rendering_does_not_require_the_values() {
            let sql = rewrite("SELECT id FROM t WHERE a = $1", |w| {
                w.filter_by("b = $1");
            })
            .unwrap();

            assert_eq!(sql, "SELECT id FROM t WHERE a = $1 AND b = $2");
        }

        /// A base query that skips a number claims two values and uses one.
        /// PostgreSQL refuses this as well, for the same reason.
        #[test]
        fn a_base_query_that_skips_a_number_is_refused() {
            let error = rewrite("SELECT id FROM t WHERE a = $2", |_| {}).unwrap_err();

            assert!(matches!(error, Error::Orphaned), "{error:?}");
        }

        #[test]
        fn postgres_renumbers_the_fragment_and_leaves_the_base_alone() {
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

        #[test]
        fn postgres_allows_one_value_in_two_places() {
            let sql = rewrite("SELECT id FROM users", |w| {
                w.filter_by("a = $1 OR b = $1");
            })
            .unwrap();

            assert_eq!(sql, "SELECT id FROM users WHERE a = $1 OR b = $1");
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
    }
}

#[cfg(feature = "sqlite")]
mod sqlite {
    use sqlx::Sqlite;
    use sqlx_query::{Error, QueryWriter};

    /// Rewrites `sql` and returns what came out.
    fn rewrite(sql: &str, apply: impl FnOnce(&mut QueryWriter<Sqlite>)) -> Result<String, Error> {
        let mut writer = QueryWriter::<Sqlite>::new(sql)?;
        apply(&mut writer);
        writer.sql()
    }

    /// `?N`, which names a value exactly as `$N` does -- so SQLite behaves
    /// like PostgreSQL and not like the other driver that spells it `?`.
    mod placeholders {
        use super::rewrite;
        use sqlx_query::Error;

        /// Bare `?` on the way in, numbered on the way out. The base query is
        /// written the way anyone would write it; the numbering is this crate's.
        #[test]
        fn sqlite_numbers_placeholders_that_arrived_bare() {
            let sql = rewrite("SELECT id FROM users WHERE tenant_id = ?", |w| {
                w.filter_by("role = ?");
            })
            .unwrap();

            assert_eq!(
                sql,
                "SELECT id FROM users WHERE tenant_id = ?1 AND role = ?2"
            );
        }

        #[test]
        fn sqlite_does_the_same_with_question_marks() {
            let sql = rewrite("SELECT id FROM users WHERE tenant_id = ? LIMIT ?", |w| {
                w.filter_by("role = ?");
            })
            .unwrap();

            assert_eq!(
                sql,
                "SELECT id FROM users WHERE tenant_id = ?1 AND role = ?3 LIMIT ?2"
            );
        }

        /// And so does SQLite, now that it is numbered. It could not when the
        /// output was a bare `?`.
        #[test]
        fn sqlite_allows_one_value_in_two_places() {
            let sql = rewrite("SELECT id FROM users", |w| {
                w.filter_by("a = ?1 OR b = ?1");
            })
            .unwrap();

            assert_eq!(sql, "SELECT id FROM users WHERE a = ?1 OR b = ?1");
        }

        #[test]
        fn ordering_may_move_a_limit_placeholder() {
            let sql = rewrite("SELECT id FROM users LIMIT ?", |w| {
                w.order_by("? asc");
            })
            .unwrap();

            assert_eq!(sql, "SELECT id FROM users ORDER BY ?2 ASC LIMIT ?1");
        }

        /// Replacing the limit deletes the placeholder that was in it, so the
        /// value bound for it would have nowhere to go. Numbering cannot help
        /// with a placeholder that is simply gone.
        #[test]
        fn replacing_a_limit_that_held_a_placeholder_is_refused() {
            let error = rewrite("SELECT id FROM users WHERE t = ? LIMIT ?", |w| {
                w.filter_by("role = ?").limit(50);
            })
            .unwrap_err();

            assert!(matches!(error, Error::Orphaned), "{error:?}");
        }

        #[test]
        fn replacing_a_literal_limit_is_fine() {
            let sql = rewrite("SELECT id FROM users WHERE t = ? LIMIT 10", |w| {
                w.filter_by("role = ?").limit(50);
            })
            .unwrap();

            assert_eq!(
                sql,
                "SELECT id FROM users WHERE t = ?1 AND role = ?2 LIMIT 50"
            );
        }

        #[test]
        fn a_question_mark_in_a_string_literal_is_not_a_placeholder() {
            let sql = rewrite("SELECT id FROM users WHERE note = '? ?'", |w| {
                w.filter_by("role = ?");
            })
            .unwrap();

            assert_eq!(sql, "SELECT id FROM users WHERE note = '? ?' AND role = ?1");
        }
    }

    /// Against a database. In memory, so this needs no server and runs
    /// wherever the rest of the suite does.
    mod database {
        use sqlx::{Row, SqlitePool};
        use sqlx_query::QueryWriter;

        const SCHEMA: &str = "
            CREATE TABLE users (
                id        INTEGER PRIMARY KEY,
                tenant_id INTEGER NOT NULL,
                name      TEXT    NOT NULL,
                role      TEXT    NOT NULL
            )
        ";

        #[derive(sqlx::FromRow, Debug, PartialEq, Eq)]
        struct User {
            id: i64,
            name: String,
        }

        async fn seed() -> SqlitePool {
            let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
            sqlx::query(SCHEMA).execute(&pool).await.unwrap();

            for (id, tenant, name, role) in [
                (1, 1, "ada", "admin"),
                (2, 1, "grace", "admin"),
                (3, 1, "alan", "member"),
                (4, 2, "edsger", "admin"),
            ] {
                sqlx::query("INSERT INTO users (id, tenant_id, name, role) VALUES (?, ?, ?, ?)")
                    .bind(id)
                    .bind(tenant)
                    .bind(name)
                    .bind(role)
                    .execute(&pool)
                    .await
                    .unwrap();
            }

            pool
        }

        /// The base query's value and the fragment's are bound in that order, and each
        /// lands on its own placeholder -- the thing a renumbering bug would break
        /// without any SQL error to show for it.
        #[tokio::test]
        async fn a_filter_binds_its_own_value() {
            let pool = seed().await;

            let mut writer =
                QueryWriter::<sqlx::Sqlite>::new("SELECT id, name FROM users WHERE tenant_id = ?")
                    .unwrap();
            writer.bind(1_i64).filter_by("role = ?").bind("admin");

            let users: Vec<User> = writer
                .build_as::<User>()
                .unwrap()
                .fetch_all(&pool)
                .await
                .unwrap();

            assert_eq!(
                users,
                vec![
                    User {
                        id: 1,
                        name: "ada".into()
                    },
                    User {
                        id: 2,
                        name: "grace".into()
                    },
                ]
            );
        }

        /// The case numbering exists for.
        ///
        /// The filter renders between `tenant_id = ?` and `LIMIT ?`. Left bare, the
        /// second placeholder in the text would be `role` while the second value given
        /// is the page size -- so the limit would land on `role` and `admin` on
        /// `LIMIT`. Numbering the output says which value each one means, and nothing
        /// has to be reordered.
        #[tokio::test]
        async fn numbering_keeps_each_value_on_its_own_placeholder() {
            let pool = seed().await;

            let mut writer = QueryWriter::<sqlx::Sqlite>::new(
                "SELECT id, name FROM users WHERE tenant_id = ? ORDER BY id LIMIT ?",
            )
            .unwrap();
            writer
                .bind(1_i64) // base: tenant_id
                .bind(2_i64) // base: limit
                .filter_by("role = ?")
                .bind("admin"); // fragment

            assert_eq!(
                writer.sql().unwrap(),
                "SELECT id, name FROM users WHERE tenant_id = ?1 AND role = ?3 ORDER BY id LIMIT ?2"
            );

            let users: Vec<User> = writer
                .build_as::<User>()
                .unwrap()
                .fetch_all(&pool)
                .await
                .unwrap();

            // Tenant 1 has two admins and one member; the limit of 2 is not what
            // trimmed this, but a mis-bound limit would have.
            assert_eq!(
                users,
                vec![
                    User {
                        id: 1,
                        name: "ada".into()
                    },
                    User {
                        id: 2,
                        name: "grace".into()
                    },
                ]
            );
        }

        /// The same shape, with a limit small enough that binding it to the wrong
        /// placeholder could not go unnoticed.
        #[tokio::test]
        async fn a_renumbered_limit_still_limits() {
            let pool = seed().await;

            let mut writer = QueryWriter::<sqlx::Sqlite>::new(
                "SELECT id, name FROM users WHERE tenant_id = ? ORDER BY id LIMIT ?",
            )
            .unwrap();
            writer
                .bind(1_i64)
                .bind(1_i64)
                .filter_by("role = ?")
                .bind("admin");

            let users: Vec<User> = writer
                .build_as::<User>()
                .unwrap()
                .fetch_all(&pool)
                .await
                .unwrap();

            assert_eq!(
                users,
                vec![User {
                    id: 1,
                    name: "ada".into()
                }]
            );
        }

        /// Ordering by the fragment first, with the base `ORDER BY id` behind it.
        #[tokio::test]
        async fn ordering_puts_the_fragment_first() {
            let pool = seed().await;

            let mut writer =
                QueryWriter::<sqlx::Sqlite>::new("SELECT id, name FROM users ORDER BY id").unwrap();
            writer.order_by("name asc");

            let users: Vec<User> = writer
                .build_as::<User>()
                .unwrap()
                .fetch_all(&pool)
                .await
                .unwrap();

            let names: Vec<&str> = users.iter().map(|u| u.name.as_str()).collect();
            assert_eq!(names, ["ada", "alan", "edsger", "grace"]);
        }

        #[tokio::test]
        async fn limit_applies() {
            let pool = seed().await;

            let mut writer =
                QueryWriter::<sqlx::Sqlite>::new("SELECT id, name FROM users ORDER BY id").unwrap();
            writer.limit(2);

            let users: Vec<User> = writer
                .build_as::<User>()
                .unwrap()
                .fetch_all(&pool)
                .await
                .unwrap();

            assert_eq!(users.len(), 2);
        }

        /// The `OR` case, executed rather than compared as text. Without the
        /// parentheses this returns every admin in every tenant, which is a data leak
        /// and not a syntax error -- nothing in the SQL would look wrong.
        #[tokio::test]
        async fn an_or_in_the_base_query_keeps_its_grouping() {
            let pool = seed().await;

            let mut writer = QueryWriter::<sqlx::Sqlite>::new(
                "SELECT id, name FROM users WHERE name = 'edsger' OR name = 'alan'",
            )
            .unwrap();
            writer.filter_by("tenant_id = ?").bind(1_i64);

            let users: Vec<User> = writer
                .build_as::<User>()
                .unwrap()
                .fetch_all(&pool)
                .await
                .unwrap();

            // Only alan: edsger is in tenant 2, and the tenant filter applies to both
            // sides of the OR rather than just the last one.
            assert_eq!(
                users,
                vec![User {
                    id: 3,
                    name: "alan".into()
                }]
            );
        }

        /// `build` rather than `build_as`, to cover the other constructor.
        #[tokio::test]
        async fn build_returns_a_runnable_query() {
            let pool = seed().await;

            let mut writer = QueryWriter::<sqlx::Sqlite>::new(
                "SELECT count(*) AS n FROM users WHERE tenant_id = ?",
            )
            .unwrap();
            writer.bind(1_i64);

            let row = writer.build().unwrap().fetch_one(&pool).await.unwrap();

            assert_eq!(row.get::<i64, _>("n"), 3);
        }
    }
}
