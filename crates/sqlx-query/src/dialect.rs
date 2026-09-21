/// Names the driver and whether its placeholders are positional (`$1`,
/// `$2`, ...) or a repeated bare marker (`?`), which the sentinel
/// placeholder-shift logic needs: only positional placeholders carry a
/// number that can be shifted by an offset.
///
/// `DB` (`Postgres`, `MySql`, `Sqlite`, ...) is a zero-sized marker type
/// with no `Default` impl in sqlx, and [`QueryComposer`](crate::QueryComposer)
/// never holds an instance of it — `DB` only ever appears as a type
/// parameter. So, unlike the rest of this crate's public API, `positional`
/// is an associated function rather than a `&self` method: there is no
/// value of type `DB` to call it on.
pub trait QueryDialect: sqlx::Database {
    fn positional() -> bool;
}

impl QueryDialect for sqlx::Postgres {
    fn positional() -> bool {
        true
    }
}

impl QueryDialect for sqlx::MySql {
    fn positional() -> bool {
        false
    }
}

impl QueryDialect for sqlx::Sqlite {
    fn positional() -> bool {
        false
    }
}
