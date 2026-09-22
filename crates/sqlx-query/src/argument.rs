use sqlx::query::Query;

use crate::Value;

/// A value that can be bound to a placeholder without the composer
/// knowing what type it is.
///
/// Blanket-implemented for everything sqlx can encode, so nothing in a
/// caller's code implements this: `uuid::Uuid`, `serde_json::Value`, a
/// `#[derive(sqlx::Type)]` newtype and [`Value`] itself all pick it up
/// from the impl below. Its only job is to be object-safe, which
/// `sqlx::Encode` isn't — that's what lets one list hold parameters of
/// different types.
pub trait Bindable<DB: sqlx::Database>: Send {
    /// What this was before it was erased, for
    /// [`QueryArgument`]'s `Debug` — there's nothing else to show.
    fn type_name(&self) -> &'static str;

    /// Binds this value onto `query`. Takes `Box<Self>` because encoding
    /// consumes the value, which is also why composing consumes the
    /// composer.
    fn bind_to(
        self: Box<Self>,
        query: Query<'static, DB, DB::Arguments>,
    ) -> Query<'static, DB, DB::Arguments>;
}

impl<DB, T> Bindable<DB> for T
where
    DB: sqlx::Database,
    T: sqlx::Encode<'static, DB> + sqlx::Type<DB> + Send + 'static,
{
    fn type_name(&self) -> &'static str {
        std::any::type_name::<T>()
    }

    fn bind_to(
        self: Box<Self>,
        query: Query<'static, DB, DB::Arguments>,
    ) -> Query<'static, DB, DB::Arguments> {
        query.bind(*self)
    }
}

/// One thing supplied for one of the base query's placeholders, in the
/// two shapes [`QueryComposer`](crate::QueryComposer) accepts it.
///
/// Both are bound the same way in the end — [`Value`] is `Encode` too, so
/// it satisfies `Bindable` as well. The split is not about how to bind but
/// about what can be *looked at*: whether
/// [`compose`](crate::QueryComposer::compose) can report the value or only
/// that there is one.
pub enum QueryArgument<DB: sqlx::Database> {
    /// A scalar the composer can show back: from
    /// [`bind_value`](crate::QueryComposer::bind_value), or from a clause,
    /// whose literals have to stay concrete anyway since a
    /// [`Cursor`](crate::Cursor) serializes them into a page token.
    Value(Value),
    /// Something only sqlx can make sense of, from
    /// [`bind`](crate::QueryComposer::bind). Bound like any other
    /// argument; there is simply nothing to report about it until
    /// [`build`](crate::QueryComposer::build) encodes it.
    Opaque(Box<dyn Bindable<DB>>),
}

impl<DB: sqlx::Database> QueryArgument<DB> {
    /// The scalar behind this argument, or `None` for one bound opaquely.
    #[must_use]
    pub fn value(&self) -> Option<&Value> {
        match self {
            QueryArgument::Value(value) => Some(value),
            QueryArgument::Opaque(_) => None,
        }
    }
}

/// Binding needs `Value` to be encodable, which is the whole reason
/// `value.rs` implements sqlx's traits for it — without that, this arm
/// would be a match over every variant.
impl<DB> QueryArgument<DB>
where
    DB: sqlx::Database,
    Value: sqlx::Encode<'static, DB> + sqlx::Type<DB>,
{
    pub(crate) fn bind_to(
        self,
        query: Query<'static, DB, DB::Arguments>,
    ) -> Query<'static, DB, DB::Arguments> {
        match self {
            QueryArgument::Value(value) => query.bind(value),
            QueryArgument::Opaque(bind) => bind.bind_to(query),
        }
    }
}

impl<DB: sqlx::Database> std::fmt::Debug for QueryArgument<DB> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            QueryArgument::Value(value) => f.debug_tuple("Value").field(value).finish(),
            QueryArgument::Opaque(bind) => {
                f.debug_tuple("Opaque").field(&bind.type_name()).finish()
            }
        }
    }
}

impl<DB: sqlx::Database> From<Value> for QueryArgument<DB> {
    fn from(value: Value) -> Self {
        QueryArgument::Value(value)
    }
}
