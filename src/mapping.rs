//! Which columns a request is allowed to name, and what they hold.

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::fmt;

/// The SQL type family of a column.
///
/// Coarse on purpose. This is not a model of the database's type system; it is
/// the smallest thing that can tell a comparison that will work from one that
/// will not, before the database is asked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ColumnType {
    /// `BOOLEAN`.
    Bool,
    /// Any signed integer type. Bound as `i64`.
    Int,
    /// Any floating-point type. Bound as `f64`.
    Float,
    /// Any character type.
    Text,
    /// `BYTEA`, `BLOB`, `VARBINARY`.
    Bytes,
    /// A timestamp, with or without a zone. Bound as `DateTime<Utc>`.
    Timestamp,
}

impl fmt::Display for ColumnType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Bool => "bool",
            Self::Int => "int",
            Self::Float => "float",
            Self::Text => "text",
            Self::Bytes => "bytes",
            Self::Timestamp => "timestamp",
        })
    }
}

/// One column a request is allowed to name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Column {
    /// The column name as the database spells it, *unquoted*. The dialect adds
    /// the quoting, so a name containing the quote character is escaped rather
    /// than becoming an injection point.
    pub name: Cow<'static, str>,
    /// What the column holds.
    pub ty: ColumnType,
    /// Whether the column is unique.
    ///
    /// Only keyset pagination cares: a cursor is only well defined when the
    /// sort it pages through is total, which needs a unique column among its
    /// keys. Declaring it here is what turns that requirement from a comment
    /// into a check.
    pub unique: bool,
}

impl Column {
    /// A column that is not unique.
    #[must_use]
    pub fn new(name: impl Into<Cow<'static, str>>, ty: ColumnType) -> Self {
        Self {
            name: name.into(),
            ty,
            unique: false,
        }
    }

    /// A column that is unique, and so can end a sort.
    #[must_use]
    pub fn key(name: impl Into<Cow<'static, str>>, ty: ColumnType) -> Self {
        Self {
            unique: true,
            ..Self::new(name, ty)
        }
    }
}

/// The allow-list.
///
/// Fail-closed: a path this returns `None` for is rejected, so an unconfigured
/// mapping exposes nothing rather than everything.
///
/// `path` arrives already split on `.`, so a request naming `author.name` is
/// asked about `["author", "name"]`. Flattening that to one column, mapping it
/// to a JSON extraction, or refusing it are all yours to decide.
///
/// [`QueryMapping`] covers the static case. Anything decided per request -- per-tenant
/// visibility, a permission check, a column only some callers may sort on -- is
/// a closure:
///
/// ```
/// use sqlx_query::{Column, ColumnType, Mapping};
///
/// let admin = false;
/// let visible = |path: &[&str]| match path {
///     ["id"] => Some(Column::key("id", ColumnType::Int)),
///     ["salary"] if admin => Some(Column::new("salary", ColumnType::Int)),
///     _ => None,
/// };
///
/// assert!(visible.resolve(&["id"]).is_some());
/// assert!(visible.resolve(&["salary"]).is_none());
/// ```
pub trait Mapping {
    /// Resolve a request path to a column, or `None` to reject it.
    fn resolve(&self, path: &[&str]) -> Option<Column>;
}

/// Note this rules out a blanket `impl Mapping for &S`: `&F` is itself `Fn` when
/// `F` is, so the two would overlap. Since the API takes `&S` everywhere, the
/// forwarding impl only ever mattered to a caller holding a `&QueryMapping` who wanted
/// `S` to be the reference itself.
impl<F: Fn(&[&str]) -> Option<Column>> Mapping for F {
    fn resolve(&self, path: &[&str]) -> Option<Column> {
        self(path)
    }
}

/// A [`Mapping`] built from an explicit list of columns.
///
/// ```
/// use sqlx_query::{Column, ColumnType, QueryMapping};
///
/// let volumes = QueryMapping::new()
///     .key("id", ColumnType::Int)
///     .column("title", ColumnType::Text)
///     .add("readCount", Column::new("read_count", ColumnType::Int));
/// ```
///
/// The request-facing name is on the left and the [`Column`] on the right,
/// which is the same shape as [`Mapping::resolve`] and leaves no doubt about
/// which spelling is which. Attributes belong to the `Column`, so a key that is
/// also aliased is `add("id", Column::key("volume_id", ..))` rather than a
/// fourth method.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct QueryMapping {
    columns: BTreeMap<String, Column>,
}

impl QueryMapping {
    /// An empty table. Every path is rejected until one is added.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Expose `field` as `column`.
    #[must_use]
    pub fn add(mut self, field: impl Into<String>, column: Column) -> Self {
        self.columns.insert(field.into(), column);
        self
    }

    /// Expose `field`, spelled the same on both sides.
    #[must_use]
    pub fn column(self, field: impl Into<String>, ty: ColumnType) -> Self {
        let field = field.into();
        let column = Column::new(field.clone(), ty);
        self.add(field, column)
    }

    /// Expose `field` as a unique column, spelled the same on both sides.
    #[must_use]
    pub fn key(self, field: impl Into<String>, ty: ColumnType) -> Self {
        let field = field.into();
        let column = Column::key(field.clone(), ty);
        self.add(field, column)
    }
}

impl Mapping for QueryMapping {
    fn resolve(&self, path: &[&str]) -> Option<Column> {
        self.columns.get(&path.join(".")).cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_table_resolves_nothing() {
        assert!(QueryMapping::new().resolve(&["id"]).is_none());
    }

    #[test]
    fn a_field_may_be_spelled_differently_from_its_column() {
        let table =
            QueryMapping::new().add("readCount", Column::new("read_count", ColumnType::Int));

        assert_eq!(table.resolve(&["readCount"]).unwrap().name, "read_count");
        assert!(table.resolve(&["read_count"]).is_none());
    }

    /// The combination `aliased` could not express: a primary key the request
    /// spells differently from the database.
    #[test]
    fn a_key_may_also_be_aliased() {
        let table = QueryMapping::new().add("id", Column::key("volume_id", ColumnType::Int));
        let column = table.resolve(&["id"]).unwrap();

        assert_eq!(column.name, "volume_id");
        assert!(column.unique);
    }

    #[test]
    fn a_nested_path_resolves_by_its_dotted_name() {
        let table =
            QueryMapping::new().add("author.name", Column::new("author_name", ColumnType::Text));

        assert_eq!(
            table.resolve(&["author", "name"]).unwrap().name,
            "author_name"
        );
        assert!(table.resolve(&["author"]).is_none());
    }

    #[test]
    fn only_key_marks_a_column_unique() {
        let table = QueryMapping::new()
            .key("id", ColumnType::Int)
            .column("title", ColumnType::Text);

        assert!(table.resolve(&["id"]).unwrap().unique);
        assert!(!table.resolve(&["title"]).unwrap().unique);
    }
}
