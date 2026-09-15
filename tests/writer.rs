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
                w.filter("role = 'admin'");
            })
            .unwrap();

            assert_eq!(sql, "SELECT id FROM users WHERE role = 'admin'");
        }

        #[test]
        fn filter_joins_an_existing_where() {
            let sql = rewrite("SELECT id FROM users WHERE tenant_id = $1", |w| {
                w.filter("role = 'admin'");
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
                w.filter("role = 'admin'")
                    .filter("active")
                    .filter("age > 18");
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
                w.filter("role = 'admin'");
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
                w.filter("role = 'admin' OR role = 'owner'");
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
                w.filter("c = 3");
            })
            .unwrap();

            assert_eq!(sql, "SELECT id FROM users WHERE a = 1 AND b = 2 AND c = 3");
        }

        #[test]
        fn order_by_goes_in_front_and_the_base_becomes_a_tiebreaker() {
            let sql = rewrite("SELECT id FROM users ORDER BY id", |w| {
                w.sort("name desc");
            })
            .unwrap();

            assert_eq!(sql, "SELECT id FROM users ORDER BY name DESC, id");
        }

        #[test]
        fn order_by_takes_a_list() {
            let sql = rewrite("SELECT id FROM users", |w| {
                w.sort("name desc, created_at asc");
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
                w.sort("name desc").sort("created_at asc");
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
                w.sort("id asc");
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
                w.sort("name asc").sort("name desc");
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
                w.filter("role = 'admin'; DROP TABLE users");
            })
            .unwrap_err();

            assert!(matches!(error, Error::Trailing { .. }), "{error:?}");
        }

        #[test]
        fn a_trailing_fragment_in_order_by_is_refused() {
            let error = rewrite("SELECT id FROM users", |w| {
                w.sort("name asc; DROP TABLE users");
            })
            .unwrap_err();

            assert!(matches!(error, Error::Trailing { .. }), "{error:?}");
        }

        #[test]
        fn a_fragment_that_is_not_an_expression_is_refused() {
            let error = rewrite("SELECT id FROM users", |w| {
                w.filter("= = =");
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
                w.filter("FROM WHERE");
            })
            .unwrap_err();

            assert!(matches!(error, Error::Trailing { .. }), "{error:?}");
        }

        #[test]
        fn a_union_has_no_single_select_to_filter() {
            let error = rewrite("SELECT id FROM a UNION SELECT id FROM b", |w| {
                w.filter("role = 'admin'");
            })
            .unwrap_err();

            assert!(matches!(error, Error::SetOperation), "{error:?}");
        }

        /// Ordering a union is unambiguous -- it applies to the whole result -- so it
        /// is allowed even though filtering one is not.
        #[test]
        fn a_union_can_still_be_ordered() {
            let sql = rewrite("SELECT id FROM a UNION SELECT id FROM b", |w| {
                w.sort("id desc");
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
                w.filter("count(*) > 5");
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
                w.filter("active");
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
            writer.filter("role = 'admin'; DROP TABLE users");

            assert!(matches!(writer.sql(), Err(Error::Trailing { .. })));
            assert!(matches!(writer.sql(), Err(Error::Trailing { .. })));
        }

        /// And the first one is the one that explains the rest.
        #[test]
        fn the_first_failure_wins() {
            let mut writer = QueryWriter::<Postgres>::new("SELECT id FROM users").unwrap();
            writer
                .filter("= = =")
                .filter("role = 'admin'; DROP TABLE users");

            assert!(matches!(writer.sql(), Err(Error::Fragment { .. })));
        }
    }

    /// AIP-132 ordering. Nothing here is PostgreSQL-specific -- `Sort` has no
    /// driver at all until a query renders it -- but the file is laid out by
    /// driver, so it lives with the one that renders it below.
    mod sort {
        use sqlx_query::{Error, Sort, SortDirection, SortKey};
        use std::collections::HashMap;

        fn columns() -> HashMap<&'static str, &'static str> {
            HashMap::from([
                ("title", "title"),
                ("readCount", "read_count"),
                ("created", "v.created_at"),
                ("id", "id"),
            ])
        }

        /// What the sort actually holds, which is the only thing that matters
        /// before it is rendered.
        fn keys(sort: &Sort) -> Vec<(String, SortDirection)> {
            sort.keys()
                .iter()
                .map(|key| (key.name.clone(), key.direction))
                .collect()
        }

        #[test]
        fn a_field_on_its_own_is_ascending() {
            let sort = Sort::parse("title").unwrap();
            assert_eq!(keys(&sort), [("title".into(), SortDirection::Asc)]);
        }

        #[test]
        fn a_direction_is_read_whichever_way_it_is_written() {
            for input in ["title desc", "title DESC", "title Desc"] {
                let sort = Sort::parse(input).unwrap();
                assert_eq!(
                    keys(&sort),
                    [("title".into(), SortDirection::Desc)],
                    "{input}"
                );
            }
        }

        #[test]
        fn terms_keep_the_order_they_arrived_in() {
            let sort = Sort::parse("readCount desc, title").unwrap();
            assert_eq!(
                keys(&sort),
                [
                    ("readCount".into(), SortDirection::Desc),
                    ("title".into(), SortDirection::Asc),
                ]
            );
        }

        #[test]
        fn whitespace_is_not_significant() {
            let sort = Sort::parse("  readCount   desc ,title  ").unwrap();
            assert_eq!(
                keys(&sort),
                [
                    ("readCount".into(), SortDirection::Desc),
                    ("title".into(), SortDirection::Asc),
                ]
            );
        }

        /// An absent query parameter arrives as a blank string, not as an
        /// error.
        #[test]
        fn a_blank_value_means_no_ordering() {
            for input in ["", "   "] {
                assert!(Sort::parse(input).unwrap().is_empty(), "{input:?}");
            }
        }

        // -- the tiebreaker ------------------------------------------------

        #[test]
        fn asc_appends_a_field() {
            let sort = Sort::parse("title desc").unwrap().asc("id");
            assert_eq!(
                keys(&sort),
                [
                    ("title".into(), SortDirection::Desc),
                    ("id".into(), SortDirection::Asc),
                ]
            );
        }

        #[test]
        fn desc_appends_a_field() {
            let sort = Sort::parse("title").unwrap().desc("id");
            assert_eq!(
                keys(&sort),
                [
                    ("title".into(), SortDirection::Asc),
                    ("id".into(), SortDirection::Desc),
                ]
            );
        }

        /// A tiebreaker follows a client rather than overruling one: the
        /// request asked for `id` descending, and keeps it.
        #[test]
        fn a_field_the_request_already_named_is_left_where_it_is() {
            let sort = Sort::parse("id desc").unwrap().asc("id");
            assert_eq!(keys(&sort), [("id".into(), SortDirection::Desc)]);
        }

        #[test]
        fn a_tiebreaker_alone_is_the_whole_ordering() {
            let sort = Sort::parse("").unwrap().asc("id");
            assert_eq!(keys(&sort), [("id".into(), SortDirection::Asc)]);
        }

        // -- resolution ----------------------------------------------------

        #[test]
        fn resolve_renames_fields_to_columns() {
            let sort = Sort::parse("readCount desc")
                .unwrap()
                .resolve(&columns())
                .unwrap();

            assert_eq!(keys(&sort), [("read_count".into(), SortDirection::Desc)]);
        }

        #[test]
        fn a_field_the_map_does_not_name_is_refused() {
            let error = Sort::parse("password_hash")
                .unwrap()
                .resolve(&columns())
                .unwrap_err();

            assert!(
                matches!(&error, Error::Field(field) if field == "password_hash"),
                "{error:?}"
            );
        }

        /// The allowlist is over fields, so asking for the column behind one is
        /// refused too -- otherwise the map would be a suggestion.
        #[test]
        fn naming_the_column_instead_of_the_field_is_refused() {
            let error = Sort::parse("read_count")
                .unwrap()
                .resolve(&columns())
                .unwrap_err();

            assert!(matches!(error, Error::Field(_)), "{error:?}");
        }

        #[test]
        fn a_tiebreaker_is_resolved_like_anything_else() {
            let error = Sort::parse("title")
                .unwrap()
                .asc("nope")
                .resolve(&columns())
                .unwrap_err();

            assert!(matches!(error, Error::Field(_)), "{error:?}");
        }

        #[test]
        fn resolving_twice_changes_nothing() {
            let once = Sort::parse("title").unwrap().resolve(&columns()).unwrap();
            let twice = once.clone().resolve(&columns()).unwrap();

            assert_eq!(keys(&once), keys(&twice));
        }

        /// An ordering this program decided has no client input in it, so
        /// there is nothing for an allowlist to check.
        #[test]
        fn a_sort_built_directly_needs_no_resolving() {
            let sort = Sort::new(vec![SortKey {
                name: "created_at".into(),
                direction: SortDirection::Desc,
            }]);

            assert_eq!(keys(&sort), [("created_at".into(), SortDirection::Desc)]);
        }

        // -- malformed -----------------------------------------------------

        #[test]
        fn a_direction_that_is_not_one_is_refused() {
            let error = Sort::parse("title sideways").unwrap_err();
            assert!(matches!(error, Error::Sort(_)), "{error:?}");
        }

        #[test]
        fn an_empty_term_is_refused() {
            for input in ["title,", ",title", "title,,created"] {
                let error = Sort::parse(input).unwrap_err();
                assert!(matches!(error, Error::Sort(_)), "{input:?} gave {error:?}");
            }
        }

        #[test]
        fn a_third_word_is_refused() {
            let error = Sort::parse("title desc extra").unwrap_err();
            assert!(matches!(error, Error::Sort(_)), "{error:?}");
        }
    }

    /// A resolved `Sort` handed to the writer, rather than a fragment.
    mod sorting {
        use sqlx::Postgres;
        use sqlx_query::{Error, QueryWriter, Sort};
        use std::collections::HashMap;

        fn columns() -> HashMap<&'static str, &'static str> {
            HashMap::from([
                ("title", "title"),
                ("readCount", "read_count"),
                ("created", "v.created_at"),
                ("order", "order"),
                ("id", "id"),
            ])
        }

        fn sorted(base: &str, order_by: &str, tiebreak: &str) -> Result<String, Error> {
            let sort = Sort::parse(order_by)?.asc(tiebreak).resolve(&columns())?;
            let mut query = QueryWriter::<Postgres>::new(base)?;
            query.sort(&sort);
            query.sql()
        }

        #[test]
        fn a_sort_becomes_the_ordering() {
            let sql = sorted("SELECT id FROM v", "title desc", "id").unwrap();
            assert_eq!(sql, r#"SELECT id FROM v ORDER BY "title" DESC, "id" ASC"#);
        }

        /// A qualified column is two identifiers, not one with a dot in it.
        #[test]
        fn a_qualified_column_is_quoted_in_parts() {
            let sql = sorted("SELECT id FROM v", "created", "id").unwrap();
            assert_eq!(
                sql,
                r#"SELECT id FROM v ORDER BY "v"."created_at" ASC, "id" ASC"#
            );
        }

        /// Why columns are quoted at all.
        #[test]
        fn a_column_named_after_a_keyword_survives() {
            let sql = sorted("SELECT id FROM v", "order", "id").unwrap();
            assert_eq!(sql, r#"SELECT id FROM v ORDER BY "order" ASC, "id" ASC"#);
        }

        /// What the query already ordered by drops behind the request's
        /// ordering, exactly as a `sort` fragment does.
        #[test]
        fn the_querys_own_ordering_becomes_a_tiebreaker() {
            let sql = sorted("SELECT id FROM v ORDER BY rank", "title desc", "id").unwrap();
            assert_eq!(
                sql,
                r#"SELECT id FROM v ORDER BY "title" DESC, "id" ASC, rank"#
            );
        }

        /// Quoting is not part of a column's identity: the `"id"` a `Sort`
        /// writes and the `id` the query wrote are one column, and it is
        /// ordered by once.
        #[test]
        fn quoting_does_not_make_a_second_column() {
            let sql = sorted("SELECT id FROM v ORDER BY id", "title desc", "id").unwrap();
            assert_eq!(sql, r#"SELECT id FROM v ORDER BY "title" DESC, "id" ASC"#);
        }

        /// Every string shape reaches the same impl, including the owned one
        /// a deserialised request actually hands you.
        #[test]
        fn a_fragment_may_be_any_kind_of_string() {
            let owned = String::from("name asc");

            for sql in [
                QueryWriter::<Postgres>::new("SELECT id FROM v")
                    .unwrap()
                    .sort("name asc")
                    .sql(),
                QueryWriter::<Postgres>::new("SELECT id FROM v")
                    .unwrap()
                    .sort(&owned)
                    .sql(),
                QueryWriter::<Postgres>::new("SELECT id FROM v")
                    .unwrap()
                    .sort(owned.clone())
                    .sql(),
            ] {
                assert_eq!(sql.unwrap(), "SELECT id FROM v ORDER BY name ASC");
            }
        }

        /// What a fragment can do that a `Sort` cannot: order by an
        /// expression. `Sort` only ever names columns, so this is the reason
        /// the two are not the same type.
        #[test]
        fn a_fragment_may_order_by_an_expression() {
            let mut writer = QueryWriter::<Postgres>::new("SELECT id FROM v").unwrap();
            writer.sort("lower(name) asc, id desc");

            assert_eq!(
                writer.sql().unwrap(),
                "SELECT id FROM v ORDER BY lower(name) ASC, id DESC"
            );
        }

        /// A fragment and a `Sort` in the same query, the fragment first.
        #[test]
        fn a_fragment_and_a_sort_may_both_be_given() {
            let sort = Sort::parse("title").unwrap().resolve(&columns()).unwrap();
            let mut writer = QueryWriter::<Postgres>::new("SELECT id FROM v").unwrap();
            writer.sort("lower(name) asc").sort(&sort);

            assert_eq!(
                writer.sql().unwrap(),
                r#"SELECT id FROM v ORDER BY lower(name) ASC, "title" ASC"#
            );
        }

        /// The check that makes the allowlist worth having: a sort still
        /// holding the client's field names never reaches the query.
        #[test]
        fn an_unresolved_sort_is_refused() {
            let sort = Sort::parse("title desc").unwrap();
            let mut query = QueryWriter::<Postgres>::new("SELECT id FROM v").unwrap();
            query.sort(&sort);

            assert!(matches!(query.sql(), Err(Error::Unresolved)));
        }

        #[test]
        fn an_empty_sort_leaves_the_query_alone() {
            let sql = Sort::parse("")
                .and_then(|sort| sort.resolve(&columns()))
                .and_then(|sort| {
                    let mut query = QueryWriter::<Postgres>::new("SELECT id FROM v")?;
                    query.sort(&sort);
                    query.sql()
                })
                .unwrap();

            assert_eq!(sql, "SELECT id FROM v");
        }

        #[test]
        fn binds_and_limits_reach_the_writer() {
            let sort = Sort::parse("title").unwrap().resolve(&columns()).unwrap();
            let mut query =
                QueryWriter::<Postgres>::new("SELECT id FROM v WHERE tenant = $1").unwrap();
            query.bind(7_i64).sort(&sort).limit(50);

            assert_eq!(
                query.sql().unwrap(),
                r#"SELECT id FROM v WHERE tenant = $1 ORDER BY "title" ASC LIMIT 50"#
            );
            assert!(query.build().is_ok());
        }

        /// A predicate this program decided, alongside an ordering the client
        /// did -- the case that used to need an escape hatch between layers.
        #[test]
        fn a_fragment_and_a_sort_sit_side_by_side() {
            let sort = Sort::parse("title").unwrap().resolve(&columns()).unwrap();
            let mut query = QueryWriter::<Postgres>::new("SELECT id FROM v").unwrap();
            query.sort(&sort);
            query.filter("visible");

            assert_eq!(
                query.sql().unwrap(),
                r#"SELECT id FROM v WHERE visible ORDER BY "title" ASC"#
            );
        }
    }

    /// CEL filters. Gated on the feature, since `Filter` is.
    #[cfg(feature = "cel")]
    mod filtering {
        use sqlx::Postgres;
        use sqlx_query::{Error, Filter, QueryWriter};
        use std::collections::HashMap;

        fn columns() -> HashMap<&'static str, &'static str> {
            HashMap::from([
                ("readCount", "read_count"),
                ("title", "title"),
                ("visible", "visible"),
                ("tier", "t.tier"),
            ])
        }

        /// The condition alone, which is all these are about.
        fn filtered(cel: &str) -> Result<String, Error> {
            let filter = Filter::parse(cel)?.resolve(&columns())?;
            let mut writer = QueryWriter::<Postgres>::new("SELECT id FROM v")?;
            writer.filter(&filter);

            Ok(writer
                .sql()?
                .trim_start_matches("SELECT id FROM v")
                .trim_start()
                .trim_start_matches("WHERE ")
                .to_owned())
        }

        #[test]
        fn comparisons_become_the_operators_they_name() {
            for (cel, sql) in [
                ("readCount == 1", r#""read_count" = $1"#),
                ("readCount != 1", r#""read_count" <> $1"#),
                ("readCount < 1", r#""read_count" < $1"#),
                ("readCount <= 1", r#""read_count" <= $1"#),
                ("readCount > 1", r#""read_count" > $1"#),
                ("readCount >= 1", r#""read_count" >= $1"#),
            ] {
                assert_eq!(filtered(cel).unwrap(), sql, "{cel}");
            }
        }

        /// `100 < readCount` says the same as `readCount > 100`, and a request
        /// may write it either way round.
        #[test]
        fn a_comparison_reads_in_either_order() {
            assert_eq!(filtered("100 < readCount").unwrap(), r#""read_count" < $1"#);
        }

        #[test]
        fn a_bare_field_is_the_column_itself() {
            assert_eq!(filtered("visible").unwrap(), r#""visible""#);
        }

        #[test]
        fn a_qualified_column_is_quoted_in_parts() {
            assert_eq!(filtered(r#"tier == "gold""#).unwrap(), r#""t"."tier" = $1"#);
        }

        /// Nothing equals null in SQL, including null, so the obvious
        /// translation would match nothing at all.
        #[test]
        fn comparing_to_null_becomes_is_null() {
            assert_eq!(
                filtered("readCount == null").unwrap(),
                r#""read_count" IS NULL"#
            );
            assert_eq!(
                filtered("readCount != null").unwrap(),
                r#""read_count" IS NOT NULL"#
            );
        }

        #[test]
        fn a_list_becomes_in() {
            assert_eq!(
                filtered("readCount in [1, 2, 3]").unwrap(),
                r#""read_count" IN ($1, $2, $3)"#
            );
        }

        #[test]
        fn the_string_methods_become_like() {
            for cel in [
                r#"title.startsWith("D")"#,
                r#"title.endsWith("D")"#,
                r#"title.contains("D")"#,
            ] {
                assert_eq!(
                    filtered(cel).unwrap(),
                    r#""title" LIKE $1 ESCAPE '!'"#,
                    "{cel}"
                );
            }
        }

        // -- shape ----------------------------------------------------------

        /// Only an `OR` under an `AND` needs parentheses; anything else would
        /// be noise in SQL someone has to read.
        #[test]
        fn parentheses_appear_only_where_precedence_needs_them() {
            assert_eq!(
                filtered("readCount > 1 && visible").unwrap(),
                r#""read_count" > $1 AND "visible""#
            );
            // On its own an `OR` is the whole clause, so it needs nothing.
            assert_eq!(
                filtered("readCount > 1 || visible").unwrap(),
                r#""read_count" > $1 OR "visible""#
            );
            assert_eq!(
                filtered(r#"readCount > 1 && (title == "a" || title == "b")"#).unwrap(),
                r#""read_count" > $1 AND ("title" = $2 OR "title" = $3)"#
            );
        }

        /// But joined onto a condition the query already had, it does -- `AND`
        /// binds tighter, and without them the query would mean something else.
        #[test]
        fn an_or_is_parenthesised_when_it_joins_an_existing_where() {
            let filter = Filter::parse("readCount > 1 || visible")
                .unwrap()
                .resolve(&columns())
                .unwrap();
            let mut writer =
                QueryWriter::<Postgres>::new("SELECT id FROM v WHERE tenant = $1").unwrap();
            writer.bind(7_i64).filter(&filter);

            assert_eq!(
                writer.sql().unwrap(),
                r#"SELECT id FROM v WHERE tenant = $1 AND ("read_count" > $2 OR "visible")"#
            );
        }

        #[test]
        fn not_wraps_only_a_joined_condition() {
            assert_eq!(filtered("!visible").unwrap(), r#"NOT "visible""#);
            assert_eq!(
                filtered("!(visible && readCount > 1)").unwrap(),
                r#"NOT ("visible" AND "read_count" > $1)"#
            );
        }

        /// An absent filter adds no condition, rather than a `TRUE` for the
        /// planner to discard and a reader to wonder about.
        #[test]
        fn a_blank_filter_adds_nothing() {
            let filter = Filter::parse("").unwrap().resolve(&columns()).unwrap();
            let mut writer = QueryWriter::<Postgres>::new("SELECT id FROM v").unwrap();
            writer.filter(&filter);

            assert!(filter.is_empty());
            assert_eq!(writer.sql().unwrap(), "SELECT id FROM v");
        }

        /// A filter's placeholders continue from whatever the query already
        /// claimed, like any other fragment's.
        #[test]
        fn values_are_numbered_after_the_querys_own() {
            let filter = Filter::parse("readCount > 1 && title == \"a\"")
                .unwrap()
                .resolve(&columns())
                .unwrap();
            let mut writer =
                QueryWriter::<Postgres>::new("SELECT id FROM v WHERE tenant = $1").unwrap();
            writer.bind(7_i64).filter(&filter);

            assert_eq!(
                writer.sql().unwrap(),
                r#"SELECT id FROM v WHERE tenant = $1 AND "read_count" > $2 AND "title" = $3"#
            );
        }

        // -- refusals --------------------------------------------------------

        /// The reason the allowlist exists: a request cannot filter on a
        /// column the query did not offer.
        #[test]
        fn a_field_the_map_does_not_name_is_refused() {
            let error = Filter::parse(r#"password_hash == "x""#)
                .unwrap()
                .resolve(&columns())
                .unwrap_err();

            assert!(
                matches!(&error, Error::Field(field) if field == "password_hash"),
                "{error:?}"
            );
        }

        #[test]
        fn an_unresolved_filter_never_reaches_the_query() {
            let filter = Filter::parse("visible").unwrap();
            let mut writer = QueryWriter::<Postgres>::new("SELECT id FROM v").unwrap();
            writer.filter(&filter);

            assert!(matches!(writer.sql(), Err(Error::Unresolved)));
        }

        #[test]
        fn what_has_no_meaning_as_a_condition_is_refused() {
            for cel in [
                "readCount + 1 > 2",          // arithmetic
                "1 == 1",                     // two literals
                "readCount",                  // fine, but see below
                r#"readCount.matches("^D")"#, // regex, which is dialect-specific
                "readCount in []",            // matches nothing
                "[1, 2]",                     // not a condition at all
            ] {
                if cel == "readCount" {
                    continue;
                }
                assert!(
                    matches!(filtered(cel), Err(Error::Filter(_) | Error::Field(_))),
                    "{cel} was accepted"
                );
            }
        }

        #[test]
        fn cel_that_does_not_parse_is_refused() {
            let error = Filter::parse("readCount >").unwrap_err();
            assert!(matches!(error, Error::Filter(_)), "{error:?}");
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
            writer.bind(1_i64).filter("b = $1");

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
                w.filter("b = $1");
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
                w.filter("role = $1");
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
                w.filter("role = $1");
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
                w.filter("role = $1");
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
                w.filter("role = $1").filter("age > $1");
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
                w.filter("role = $1");
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
                w.filter("a = $1 OR b = $1");
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
                w.filter("role = $1");
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
                w.filter("role = ?");
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
                w.filter("role = ?");
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
                w.filter("a = ?1 OR b = ?1");
            })
            .unwrap();

            assert_eq!(sql, "SELECT id FROM users WHERE a = ?1 OR b = ?1");
        }

        #[test]
        fn ordering_may_move_a_limit_placeholder() {
            let sql = rewrite("SELECT id FROM users LIMIT ?", |w| {
                w.sort("? asc");
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
                w.filter("role = ?").limit(50);
            })
            .unwrap_err();

            assert!(matches!(error, Error::Orphaned), "{error:?}");
        }

        #[test]
        fn replacing_a_literal_limit_is_fine() {
            let sql = rewrite("SELECT id FROM users WHERE t = ? LIMIT 10", |w| {
                w.filter("role = ?").limit(50);
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
                w.filter("role = ?");
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
            writer.bind(1_i64).filter("role = ?").bind("admin");

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
                .filter("role = ?")
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
                .filter("role = ?")
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
            writer.sort("name asc");

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
            writer.filter("tenant_id = ?").bind(1_i64);

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

        /// A filter's values reach the database as values. The SQL is the
        /// same whatever the client searched for, which is the point.
        #[cfg(feature = "cel")]
        #[tokio::test]
        async fn a_filter_binds_its_values() {
            use sqlx_query::Filter;
            let pool = seed().await;

            let columns = std::collections::HashMap::from([
                ("role", "role"),
                ("name", "name"),
                ("tenantId", "tenant_id"),
            ]);
            let filter = Filter::parse(r#"role == "admin" && tenantId == 1"#)
                .unwrap()
                .resolve(&columns)
                .unwrap();

            let mut writer =
                QueryWriter::<sqlx::Sqlite>::new("SELECT id, name FROM users ORDER BY id").unwrap();
            writer.filter(&filter);

            let users: Vec<User> = writer
                .build_as::<User>()
                .unwrap()
                .fetch_all(&pool)
                .await
                .unwrap();

            let names: Vec<&str> = users.iter().map(|u| u.name.as_str()).collect();
            assert_eq!(names, ["ada", "grace"]);
        }

        /// The case escaping exists for. Searching for a name beginning `50%`
        /// must not match every name beginning `50` -- and only a database can
        /// say whether the escape was written correctly.
        #[cfg(feature = "cel")]
        #[tokio::test]
        async fn a_wildcard_in_a_search_term_is_not_a_wildcard() {
            use sqlx_query::Filter;
            let pool = seed().await;

            for (id, name) in [(10, "50% off"), (11, "5000 off"), (12, "a_b"), (13, "axb")] {
                sqlx::query("INSERT INTO users (id, tenant_id, name, role) VALUES (?, 1, ?, 'x')")
                    .bind(id)
                    .bind(name)
                    .execute(&pool)
                    .await
                    .unwrap();
            }

            let columns = std::collections::HashMap::from([("name", "name")]);

            for (cel, expected) in [
                (r#"name.startsWith("50%")"#, vec!["50% off"]),
                (r#"name.contains("a_b")"#, vec!["a_b"]),
            ] {
                let filter = Filter::parse(cel).unwrap().resolve(&columns).unwrap();
                let mut writer =
                    QueryWriter::<sqlx::Sqlite>::new("SELECT id, name FROM users ORDER BY id")
                        .unwrap();
                writer.filter(&filter);

                let users: Vec<User> = writer
                    .build_as::<User>()
                    .unwrap()
                    .fetch_all(&pool)
                    .await
                    .unwrap();

                let names: Vec<&str> = users.iter().map(|u| u.name.as_str()).collect();
                assert_eq!(names, expected, "{cel}");
            }
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
