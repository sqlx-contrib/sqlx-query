//! Which columns a result is ordered by, and in which direction.

use std::fmt;
use std::str::FromStr;

use crate::dialect::{Dialect, quote};
use crate::error::Error;
use crate::fragment::QueryFragment;
use crate::schema::Schema;
use crate::value::Value;

/// Which way a sort key runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Direction {
    /// Smallest first.
    #[default]
    Asc,
    /// Largest first.
    Desc,
}

impl Direction {
    /// The SQL keyword.
    #[must_use]
    pub fn keyword(self) -> &'static str {
        match self {
            Self::Asc => "ASC",
            Self::Desc => "DESC",
        }
    }
}

impl fmt::Display for Direction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.keyword())
    }
}

/// One key of a sort: a request-facing field path and a direction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SortKey {
    /// The path as the request spells it, dots and all.
    pub field: String,
    /// Which way it runs.
    pub direction: Direction,
}

impl SortKey {
    /// A key, ascending.
    #[must_use]
    pub fn asc(field: impl Into<String>) -> Self {
        Self {
            field: field.into(),
            direction: Direction::Asc,
        }
    }

    /// A key, descending.
    #[must_use]
    pub fn desc(field: impl Into<String>) -> Self {
        Self {
            field: field.into(),
            direction: Direction::Desc,
        }
    }
}

/// An ordering.
///
/// # Each field appears once
///
/// A field repeated in a sort can never affect the ordering -- by the time the
/// second occurrence is consulted the first has already decided -- but it does
/// lengthen the seek predicate, bloat a cursor, and make two orderings that
/// should compare equal compare differently. So every way of adding a key
/// drops one that is already present, and the first occurrence wins.
///
/// First rather than last, because the direction a caller asked for is the one
/// they should get. Overriding `id desc` with a later `id asc` would put the
/// `ORDER BY` at odds with the request that produced it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Sort {
    keys: Vec<SortKey>,
}

impl Sort {
    /// An ordering with no keys.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Parse an [AIP-132] `order_by` string: comma-separated field paths, each
    /// optionally followed by `asc` or `desc`.
    ///
    /// An empty or all-whitespace string is an empty ordering, not an error --
    /// that is what a request that did not ask for one looks like.
    ///
    /// ```
    /// use sqlx_query::{Direction, Sort};
    ///
    /// let sort = Sort::parse("title desc, id")?;
    ///
    /// assert_eq!(sort.keys()[0].field, "title");
    /// assert_eq!(sort.keys()[0].direction, Direction::Desc);
    /// assert_eq!(sort.keys()[1].direction, Direction::Asc);
    /// # Ok::<_, sqlx_query::Error>(())
    /// ```
    ///
    /// [AIP-132]: https://google.aip.dev/132
    ///
    /// # Errors
    ///
    /// [`Error::Sort`] for an empty element, a missing field, or a direction
    /// that is neither `asc` nor `desc`.
    pub fn parse(source: &str) -> Result<Self, Error> {
        let source = source.trim();
        if source.is_empty() {
            return Ok(Self::new());
        }

        let mut sort = Self::new();

        for element in source.split(',') {
            let element = element.trim();
            if element.is_empty() {
                return Err(Error::Sort(format!(
                    "`{source}` has an empty element: check for a stray comma"
                )));
            }

            let mut words = element.split_ascii_whitespace();
            let Some(field) = words.next() else {
                return Err(Error::Sort(format!("`{element}` names no field")));
            };

            let direction = match words.next() {
                None => Direction::Asc,
                Some(word) if word.eq_ignore_ascii_case("asc") => Direction::Asc,
                Some(word) if word.eq_ignore_ascii_case("desc") => Direction::Desc,
                Some(word) => {
                    return Err(Error::Sort(format!(
                        "`{word}` is not a direction: expected `asc` or `desc`"
                    )));
                }
            };

            if let Some(extra) = words.next() {
                return Err(Error::Sort(format!(
                    "`{element}` has trailing text (`{extra}`): expected `field [asc|desc]`"
                )));
            }

            sort = sort.push(SortKey {
                field: field.to_owned(),
                direction,
            });
        }

        Ok(sort)
    }

    /// Add an ascending key, unless the field is already one.
    ///
    /// Appending a unique column is also how a sort is made total, which keyset
    /// pagination requires: a cursor can only name an exact row if the ordering
    /// has no ties. Because a field already present is left alone, that append
    /// is safe to make unconditionally -- a caller who asked for `id desc`
    /// keeps their direction.
    ///
    /// ```
    /// use sqlx_query::{Direction, Sort};
    ///
    /// let sort = Sort::parse("title asc, id desc")?.asc("id");
    ///
    /// assert_eq!(sort.keys().len(), 2);
    /// assert_eq!(sort.keys()[1].direction, Direction::Desc);
    /// # Ok::<_, sqlx_query::Error>(())
    /// ```
    #[must_use]
    pub fn asc(self, field: impl Into<String>) -> Self {
        self.push(SortKey::asc(field))
    }

    /// Add a descending key, unless the field is already one.
    #[must_use]
    pub fn desc(self, field: impl Into<String>) -> Self {
        self.push(SortKey::desc(field))
    }

    /// The keys, in order.
    #[must_use]
    pub fn keys(&self) -> &[SortKey] {
        &self.keys
    }

    /// Whether this ordering has any keys.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    /// Render as the body of an `ORDER BY`.
    ///
    /// # Errors
    ///
    /// [`Error::UnknownColumn`] for a field the schema does not expose.
    pub fn to_fragment<DB: Dialect, S: Schema>(
        &self,
        schema: &S,
    ) -> Result<QueryFragment<DB, Value>, Error> {
        let mut fragment = QueryFragment::new();
        let mut sql = String::new();

        for (at, key) in self.keys.iter().enumerate() {
            if at > 0 {
                sql.push_str(", ");
            }

            let column = resolve(schema, &key.field)?;
            quote::<DB>(&column.name, &mut sql);
            sql.push(' ');
            sql.push_str(key.direction.keyword());
        }

        fragment.push(&sql);
        Ok(fragment)
    }

    /// Add a key unless its field is already present.
    fn push(mut self, key: SortKey) -> Self {
        self.insert(key);
        self
    }

    fn insert(&mut self, key: SortKey) {
        if !self.keys.iter().any(|existing| existing.field == key.field) {
            self.keys.push(key);
        }
    }
}

/// Resolve a dotted field path through a schema.
pub(crate) fn resolve<S: Schema>(schema: &S, field: &str) -> Result<crate::schema::Column, Error> {
    let path: Vec<&str> = field.split('.').collect();

    schema
        .resolve(&path)
        .ok_or_else(|| Error::UnknownColumn(field.to_owned()))
}

impl FromStr for Sort {
    type Err = Error;

    fn from_str(source: &str) -> Result<Self, Self::Err> {
        Self::parse(source)
    }
}

/// How a caller who already parsed an ordering gets one in -- from generated
/// AIP code, a hand-rolled parser, or a sort known at compile time. Dedup still
/// applies, so a repeated field collapses here too.
impl FromIterator<SortKey> for Sort {
    fn from_iter<I: IntoIterator<Item = SortKey>>(keys: I) -> Self {
        keys.into_iter().fold(Self::new(), Self::push)
    }
}

impl Extend<SortKey> for Sort {
    fn extend<I: IntoIterator<Item = SortKey>>(&mut self, keys: I) {
        for key in keys {
            self.insert(key);
        }
    }
}

impl fmt::Display for Sort {
    /// The AIP-132 spelling, canonical: directions lowercase, one space, one
    /// comma and a space between keys. Two orderings that mean the same thing
    /// print the same way, which is what makes comparing them safe.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (at, key) in self.keys.iter().enumerate() {
            if at > 0 {
                f.write_str(", ")?;
            }
            write!(
                f,
                "{} {}",
                key.field,
                key.direction.keyword().to_lowercase()
            )?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use sqlx::Postgres;

    use super::*;
    use crate::schema::{Column, ColumnType, Table};

    fn volumes() -> Table {
        Table::new()
            .key("id", ColumnType::Int)
            .column("title", ColumnType::Text)
            .add("readCount", Column::new("read_count", ColumnType::Int))
    }

    #[test]
    fn a_bare_field_is_ascending() {
        let sort = Sort::parse("title").unwrap();
        assert_eq!(sort.keys(), [SortKey::asc("title")]);
    }

    #[test]
    fn directions_are_case_insensitive() {
        assert_eq!(
            Sort::parse("title DESC, id Asc").unwrap().keys(),
            [SortKey::desc("title"), SortKey::asc("id")]
        );
    }

    #[test]
    fn an_absent_order_by_is_an_empty_sort_not_an_error() {
        assert!(Sort::parse("").unwrap().is_empty());
        assert!(Sort::parse("   ").unwrap().is_empty());
    }

    #[test]
    fn malformed_elements_are_rejected() {
        for source in ["title,", "title asc desc", "title sideways", "a,,b"] {
            assert!(Sort::parse(source).is_err(), "accepted `{source}`");
        }
    }

    /// A repeated field cannot affect the ordering, but it would lengthen the
    /// predicate and make equal sorts compare unequal.
    #[test]
    fn a_repeated_field_collapses_to_its_first_occurrence() {
        let sort = Sort::parse("title asc, id, title desc").unwrap();

        assert_eq!(sort.keys(), [SortKey::asc("title"), SortKey::asc("id")]);
    }

    /// The direction the caller asked for is the one they keep.
    #[test]
    fn appending_a_present_field_leaves_its_direction_alone() {
        let sort = Sort::parse("title asc, id desc").unwrap().asc("id");

        assert_eq!(sort.keys(), [SortKey::asc("title"), SortKey::desc("id")]);
    }

    #[test]
    fn appending_an_absent_field_adds_it() {
        let sort = Sort::parse("title asc").unwrap().asc("id");

        assert_eq!(sort.keys(), [SortKey::asc("title"), SortKey::asc("id")]);
    }

    #[test]
    fn collecting_from_keys_dedups_too() {
        let sort: Sort = [SortKey::desc("id"), SortKey::asc("id")]
            .into_iter()
            .collect();

        assert_eq!(sort.keys(), [SortKey::desc("id")]);
    }

    #[test]
    fn rendering_quotes_columns_and_maps_aliases() {
        let sort = Sort::parse("readCount desc, id").unwrap();
        let fragment = sort.to_fragment::<Postgres, _>(&volumes()).unwrap();

        assert_eq!(fragment.preview(), r#""read_count" DESC, "id" ASC"#);
    }

    #[test]
    fn an_unknown_field_is_rejected() {
        let error = Sort::parse("salary")
            .unwrap()
            .to_fragment::<Postgres, _>(&volumes())
            .unwrap_err();

        assert!(matches!(error, Error::UnknownColumn(field) if field == "salary"));
    }

    /// Two orderings that mean the same thing must print the same way, because
    /// that spelling is what a cursor records and later compares.
    #[test]
    fn display_is_canonical() {
        assert_eq!(
            Sort::parse("Title   DESC ,  id ASC").unwrap().to_string(),
            "Title desc, id asc"
        );
    }
}
