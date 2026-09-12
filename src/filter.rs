//! Filters a client sent.

use crate::dialect::Dialect;
use crate::error::Error;
use crate::fragment::QueryFragment;
use std::collections::BTreeMap;

use crate::mapping::{Column, Mapping};
use crate::value::Value;

/// A parsed filter expression.
///
/// Renders to a boolean expression, suitable to drop after `WHERE`, after
/// `AND`, into a `HAVING`, or into a `CHECK` -- which is why a
/// `/* AND query.filter */` slot takes one of these and a [`Cursor`]'s seek
/// condition together, joined by that slot's own `AND`.
///
/// [`Cursor`]: crate::Cursor
#[derive(Debug, Clone)]
pub struct Filter {
    expression: Option<cel::common::ast::IdedExpr>,
    /// Every path the expression names, resolved. `None` until
    /// [`resolve`](Self::resolve) has run.
    columns: Option<BTreeMap<String, Column>>,
}

impl Filter {
    /// A filter that constrains nothing.
    ///
    /// Renders to nothing at all, so the slot it fills -- and that slot's
    /// joiner -- disappear from the query.
    #[must_use]
    pub fn empty() -> Self {
        Self {
            expression: None,
            columns: Some(BTreeMap::new()),
        }
    }

    /// Parse a [CEL] expression.
    ///
    /// An empty or all-whitespace string is an empty filter, not an error, to
    /// match [`Sort::parse`] and [`Cursor::parse`]: a request that did not ask
    /// to filter should look like one that did not ask to sort.
    ///
    /// The expression is plain CEL, not the [AIP-160] grammar -- AIP is one
    /// caller with a CEL expression and a table, not a requirement.
    ///
    /// Nothing is type-checked here, because cel-rust has no checking phase:
    /// `id > 'tuesday'` parses perfectly well. The [`Mapping`] is what catches
    /// it, at [`to_fragment`](Self::to_fragment), which is why the allow-list
    /// and the type checker are the same object.
    ///
    /// [CEL]: https://cel.dev
    /// [AIP-160]: https://google.aip.dev/160
    /// [`Sort::parse`]: crate::Sort::parse
    /// [`Cursor::parse`]: crate::Cursor::parse
    ///
    /// # Errors
    ///
    /// [`Error::Parse`] for anything the CEL grammar rejects.
    pub fn parse(source: &str) -> Result<Self, Error> {
        if source.trim().is_empty() {
            return Ok(Self::empty());
        }

        Ok(Self {
            expression: Some(crate::cel::parse(source)?),
            columns: None,
        })
    }

    /// Resolve every path this filter names, through `mapping`.
    ///
    /// This is the boundary: before it a filter is a string a client sent,
    /// after it every path it names is one the mapping exposes. An unknown
    /// field fails here, on the line that handles request input, rather than
    /// when a query is built.
    ///
    /// Type mismatches -- `id > \'tuesday\'` -- are still caught when the
    /// filter renders, since that needs the shape of each comparison and not
    /// just the columns.
    ///
    /// # Errors
    ///
    /// [`Error::UnknownColumn`] for a path the mapping does not expose.
    pub fn resolve(mut self, mapping: &dyn Mapping) -> Result<Self, Error> {
        let mut columns = BTreeMap::new();

        if let Some(expression) = &self.expression {
            for path in crate::cel::fields(expression) {
                let column = crate::cel::resolve(mapping, &path)?;
                columns.insert(path, column);
            }
        }

        self.columns = Some(columns);
        Ok(self)
    }

    /// Whether this filter would constrain anything.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.expression.is_none()
    }

    /// Render against a mapping.
    ///
    /// # Errors
    ///
    /// [`Error::UnknownColumn`] for a field the mapping does not expose,
    /// [`Error::TypeMismatch`] for a comparison that cannot work, and
    /// [`Error::Unsupported`] for a construct with no faithful SQL lowering.
    pub fn to_fragment<DB: Dialect>(&self) -> Result<QueryFragment<DB, Value>, Error> {
        let columns = self.columns.as_ref().ok_or(Error::Unresolved)?;

        match &self.expression {
            None => Ok(QueryFragment::new()),
            Some(expression) => crate::cel::render(expression, columns),
        }
    }
}

impl<DB: Dialect> crate::render::Render<DB> for Filter {
    fn to_fragment(&self) -> Result<QueryFragment<DB, Value>, Error> {
        Filter::to_fragment(self)
    }
}

// Rendering is what these check, and rendering needs a driver; PostgreSQL is
// the one they are written against.
#[cfg(all(test, feature = "postgres"))]
mod tests {
    use sqlx::Postgres;

    use super::*;
    use crate::mapping::{ColumnType, QueryMapping};

    fn volumes() -> QueryMapping {
        QueryMapping::new()
            .key("id", ColumnType::Int)
            .column("title", ColumnType::Text)
    }

    /// A request that did not ask to filter should look like one that did not
    /// ask to sort: an empty string, not an error.
    #[test]
    fn an_absent_filter_is_empty_not_an_error() {
        for source in ["", "   "] {
            let predicate = Filter::parse(source).unwrap().resolve(&volumes()).unwrap();

            assert!(predicate.is_empty());
            assert!(predicate.to_fragment::<Postgres>().unwrap().is_empty());
        }
    }

    #[test]
    fn an_empty_filter_constrains_nothing() {
        assert!(Filter::empty().is_empty());
    }

    #[test]
    fn a_filter_renders_against_the_schema() {
        let fragment = Filter::parse("id > 21")
            .unwrap()
            .resolve(&volumes())
            .unwrap()
            .to_fragment::<Postgres>()
            .unwrap();

        assert_eq!(fragment.preview(), r#""id" > ?"#);
    }

    #[test]
    fn a_syntax_error_is_rejected_at_parse() {
        assert!(matches!(
            Filter::parse("id >").unwrap_err(),
            Error::Parse(_)
        ));
    }

    /// cel-rust parses without checking, so the mapping is the only thing that
    /// can catch this before the database does.
    #[test]
    fn a_type_error_is_rejected_at_render() {
        let error = Filter::parse("id > 'tuesday'")
            .unwrap()
            .resolve(&volumes())
            .unwrap()
            .to_fragment::<Postgres>()
            .unwrap_err();

        assert!(matches!(error, Error::TypeMismatch(_)), "{error}");
    }
}
