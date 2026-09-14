use sqlparser::dialect::Dialect as SqlDialect;

/// The two things a rewrite needs from a driver: how to read its SQL, and how
/// to write a placeholder back out.
///
/// This is a supertrait of [`sqlx::Database`] rather than a parallel hierarchy,
/// so `QueryWriter<Postgres>` names the same `Postgres` the rest of your
/// queries do and no adapter type stands between them.
///
/// It is sealed. Implementing it means claiming that sqlparser's dialect
/// matches what the driver will actually run, and that claim is only checkable
/// here.
// The `Arguments` bound is what lets `build` hand the collected values to
// sqlx. Every driver satisfies it -- sqlx's own macro writes the impl -- but
// nothing in `Database` says so, so it has to be said here.
pub trait Dialect: sqlx::Database<Arguments: sqlx::IntoArguments<Self>> + sealed::Sealed {
    /// The grammar the base query and its fragments are parsed with.
    fn parser() -> &'static dyn SqlDialect;

    /// Renders the placeholder that binds the `index`th value, counting from
    /// zero.
    fn placeholder(index: usize) -> String;

    /// Whether placeholders are bound by their position in the text.
    ///
    /// MySQL's `?` is: the third one in the statement takes the third value,
    /// so moving it moves what it binds, and it has no way to ask for a value
    /// twice. PostgreSQL's `$N` and SQLite's `?N` are not: each names the
    /// *N*th value wherever it appears, and may appear more than once or not
    /// at all.
    ///
    /// This decides how the values are sent. A driver that numbers takes them
    /// in the order they were given; one that does not takes them in the order
    /// the finished statement renders, which is the only thing that says which
    /// value each placeholder means.
    fn positional() -> bool;
}

mod sealed {
    pub trait Sealed {}
}

#[cfg(feature = "postgres")]
mod postgres {
    use super::{SqlDialect, sealed};
    use sqlparser::dialect::PostgreSqlDialect;

    static DIALECT: PostgreSqlDialect = PostgreSqlDialect {};

    impl sealed::Sealed for sqlx::Postgres {}

    impl super::Dialect for sqlx::Postgres {
        fn parser() -> &'static dyn SqlDialect {
            &DIALECT
        }

        fn placeholder(index: usize) -> String {
            format!("${}", index + 1)
        }

        fn positional() -> bool {
            false
        }
    }
}

#[cfg(feature = "mysql")]
mod mysql {
    use super::{SqlDialect, sealed};
    use sqlparser::dialect::MySqlDialect;

    static DIALECT: MySqlDialect = MySqlDialect {};

    impl sealed::Sealed for sqlx::MySql {}

    impl super::Dialect for sqlx::MySql {
        fn parser() -> &'static dyn SqlDialect {
            &DIALECT
        }

        fn placeholder(_index: usize) -> String {
            "?".to_owned()
        }

        fn positional() -> bool {
            true
        }
    }
}

#[cfg(feature = "sqlite")]
mod sqlite {
    use super::{SqlDialect, sealed};
    use sqlparser::dialect::SQLiteDialect;

    static DIALECT: SQLiteDialect = SQLiteDialect {};

    impl sealed::Sealed for sqlx::Sqlite {}

    impl super::Dialect for sqlx::Sqlite {
        fn parser() -> &'static dyn SqlDialect {
            &DIALECT
        }

        // `?NNN`, not bare `?`. SQLite is the only one of the three whose
        // placeholder can be both a question mark and numbered, which puts it
        // on the same footing as PostgreSQL: a placeholder names the value it
        // wants, so a fragment spliced ahead of it does not disturb it and
        // nothing has to be replayed in a different order.
        fn placeholder(index: usize) -> String {
            format!("?{}", index + 1)
        }

        fn positional() -> bool {
            false
        }
    }
}
