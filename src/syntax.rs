use sqlparser::dialect::Dialect;

/// The two things a rewrite needs from a driver: how to read its SQL, and how
/// to write a placeholder back out.
///
/// This is a supertrait of [`sqlx::Database`] rather than a parallel hierarchy,
/// so `QueryWriter<Postgres>` names the same `Postgres` the rest of your
/// queries do and no adapter type stands between them.
///
/// # Implementing it for a driver this crate does not ship
///
/// Nothing stops you. What you are claiming by doing so is that
/// [`parser`](Self::parser) accepts the same grammar the driver will actually
/// run, and that [`placeholder`](Self::placeholder) and
/// [`positional`](Self::positional) agree with how it binds. Get those wrong
/// and the rewrite produces SQL that parses here and means something else
/// there, which is not a failure any test in this crate can catch for you.
pub trait Syntax: sqlx::Database<Arguments: sqlx::IntoArguments<Self>> {
    /// The grammar the base query and its fragments are parsed with.
    fn parser() -> &'static dyn Dialect;

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

#[cfg(feature = "postgres")]
mod postgres {
    use super::{Dialect, Syntax};
    use sqlparser::dialect::PostgreSqlDialect;

    static DIALECT: PostgreSqlDialect = PostgreSqlDialect {};

    impl Syntax for sqlx::Postgres {
        fn parser() -> &'static dyn Dialect {
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
    use super::{Dialect, Syntax};
    use sqlparser::dialect::MySqlDialect;

    static DIALECT: MySqlDialect = MySqlDialect {};

    impl Syntax for sqlx::MySql {
        fn parser() -> &'static dyn Dialect {
            &DIALECT
        }

        // Bare, because MySQL has no other form: `?1` is a syntax error and
        // `$1` comes back as an unknown column. It is the reason this crate
        // has a positional path at all.
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
    use super::{Dialect, Syntax};
    use sqlparser::dialect::SQLiteDialect;

    static DIALECT: SQLiteDialect = SQLiteDialect {};

    impl Syntax for sqlx::Sqlite {
        fn parser() -> &'static dyn Dialect {
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
