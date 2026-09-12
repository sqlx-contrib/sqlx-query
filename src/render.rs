//! Things that can render themselves into a query.

use crate::dialect::Dialect;
use crate::error::Error;
use crate::fragment::QueryFragment;
use crate::mapping::Mapping;
use crate::value::Value;

/// Something a slot can be filled with.
///
/// Implemented by every producer here -- [`Filter`], [`Sort`], [`Cursor`] --
/// and by a [`QueryFragment`] built by hand. It exists so
/// [`QueryBuilder::fill`] can take the producer itself rather than a rendered
/// fragment, which keeps the mapping in one place and the builder chain free of
/// `?`.
///
/// [`Filter`]: crate::Filter
/// [`Sort`]: crate::Sort
/// [`Cursor`]: crate::Cursor
/// [`QueryBuilder::fill`]: crate::QueryBuilder::fill
pub trait Render<DB: Dialect> {
    /// Render against a mapping.
    ///
    /// # Errors
    ///
    /// Whatever the particular producer can object to: a field the mapping does
    /// not expose, a comparison that cannot work, a construct with no SQL
    /// lowering.
    fn to_fragment(&self, mapping: &dyn Mapping) -> Result<QueryFragment<DB, Value>, Error>;
}

/// A fragment built by hand renders as itself.
///
/// Cloning, because `fill` needs one it can consume and this is the rare path
/// -- everything else here is a producer that renders fresh.
impl<DB: Dialect> Render<DB> for QueryFragment<DB, Value> {
    fn to_fragment(&self, _: &dyn Mapping) -> Result<QueryFragment<DB, Value>, Error> {
        Ok(self.clone())
    }
}
