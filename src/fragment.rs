//! A rendered SQL fragment and the values behind it.

use std::fmt;
use std::marker::PhantomData;

/// SQL with bind values, ready to be spliced into a slot.
///
/// # Why the placeholders are not in here
///
/// A fragment stores the literal SQL *between* its binds, not the binds' text.
/// Nothing in here knows whether it will end up as `$3` or `?`, or what number
/// it will get, because that depends on the query it is spliced into and on how
/// many values were bound before it.
///
/// Splicing then interleaves the segments with placeholders written by
/// [`Arguments::format_placeholder`], which is the same call
/// `QueryBuilder::push_bind` makes. So the numbering is produced once, by the
/// driver, at the moment the value is added -- there is no second pass that
/// rewrites `?` into `$3`, and therefore no way for a rewrite to walk into a
/// string literal.
///
/// [`Arguments::format_placeholder`]: sqlx::Arguments::format_placeholder
///
/// # Building one
///
/// Alternate [`push`](Self::push) and [`push_bind`](Self::push_bind). Neither
/// needs a driver: `T` only has to be encodable when the fragment reaches a
/// slot, not when it is built.
///
/// ```
/// # use sqlx_query::QueryFragment;
/// # use sqlx::Postgres;
/// let mut fragment = QueryFragment::<Postgres, i64>::new();
/// fragment.push("read_count > ").push_bind(100);
/// assert!(!fragment.is_empty());
/// ```
pub struct QueryFragment<DB, T> {
    /// The literal SQL around the binds. Always `values.len() + 1` entries:
    /// segment *i* precedes value *i*, and the last segment trails them all.
    segments: Vec<String>,
    values: Vec<T>,
    database: PhantomData<DB>,
}

impl<DB, T> QueryFragment<DB, T> {
    /// An empty fragment.
    ///
    /// An empty fragment fills nothing: the slot it is given to, and that
    /// slot's joiner, both vanish from the query.
    #[must_use]
    pub fn new() -> Self {
        Self {
            segments: vec![String::new()],
            values: Vec::new(),
            database: PhantomData,
        }
    }

    /// Append literal SQL.
    ///
    /// This is not escaped or checked. Never pass untrusted input here; that is
    /// what [`push_bind`](Self::push_bind) is for.
    pub fn push(&mut self, sql: &str) -> &mut Self {
        // The trailing segment is an invariant, so the `else` is unreachable --
        // written this way rather than as an `expect` so that no public method
        // here can panic at all.
        if let Some(segment) = self.segments.last_mut() {
            segment.push_str(sql);
        }
        self
    }

    /// Append a bind value, and reserve the place its placeholder will go.
    pub fn push_bind(&mut self, value: T) -> &mut Self {
        self.values.push(value);
        self.segments.push(String::new());
        self
    }

    /// Whether this fragment would contribute anything.
    ///
    /// True for a fresh fragment, and for one that was only ever pushed empty
    /// strings. A fragment with a bind is never empty, even if it has no text.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.values.is_empty() && self.segments.iter().all(String::is_empty)
    }

    /// The number of bind values.
    #[must_use]
    pub fn len(&self) -> usize {
        self.values.len()
    }

    /// The segments and the values, for splicing.
    pub(crate) fn parts(&self) -> (&[String], &[T]) {
        (&self.segments, &self.values)
    }

    /// The SQL with `?` where each bind will go.
    ///
    /// Not what gets executed -- the driver decides the placeholder at splice
    /// time -- but the only way to look at a fragment on its own.
    pub(crate) fn preview(&self) -> String {
        self.segments.join("?")
    }
}

impl<DB, T> Default for QueryFragment<DB, T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<DB, T: fmt::Debug> fmt::Debug for QueryFragment<DB, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("QueryFragment")
            .field("segments", &self.segments)
            .field("values", &self.values)
            .finish()
    }
}

impl<DB, T: Clone> Clone for QueryFragment<DB, T> {
    fn clone(&self) -> Self {
        Self {
            segments: self.segments.clone(),
            values: self.values.clone(),
            database: PhantomData,
        }
    }
}
