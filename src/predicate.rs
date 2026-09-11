//! Boolean expressions, whatever produced them.

use crate::cursor::CursorKey;
use crate::dialect::{Dialect, quoted};
use crate::error::Error;
use crate::fragment::QueryFragment;
use crate::schema::Schema;
use crate::sort::{Direction, Sort, resolve};
use crate::value::Value;

/// A condition, suitable to drop after `WHERE`, after `AND`, into a `HAVING`,
/// or into a `CHECK`.
///
/// One type rather than one per source, because everything that fills a
/// predicate slot is the same kind of thing: a boolean expression over
/// allow-listed columns. A keyset's seek condition and a filter a client sent
/// are both this, which is why a `/* AND query.predicate */` slot takes them
/// both and joins them with its own `AND`.
///
/// Empty predicates fill nothing at all -- not even the joiner -- so a first
/// page and an absent filter both simply leave the query as it was written.
#[derive(Debug, Clone)]
pub struct Predicate {
    kind: Kind,
}

#[derive(Debug, Clone)]
enum Kind {
    /// Nothing to say.
    Empty,
    /// Rows after a keyset position.
    Seek { sort: Sort, keys: Vec<CursorKey> },
}

impl Predicate {
    /// A condition that contributes nothing.
    #[must_use]
    pub fn empty() -> Self {
        Self { kind: Kind::Empty }
    }

    /// Whether this condition would contribute anything.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        match &self.kind {
            Kind::Empty => true,
            Kind::Seek { keys, .. } => keys.is_empty(),
        }
    }

    /// Render against a schema.
    ///
    /// # Errors
    ///
    /// [`Error::UnknownColumn`] for a field the schema does not expose, and
    /// whatever else the particular condition can object to.
    pub fn to_fragment<DB: Dialect, S: Schema>(
        &self,
        schema: &S,
    ) -> Result<QueryFragment<DB, Value>, Error> {
        match &self.kind {
            Kind::Empty => Ok(QueryFragment::new()),
            Kind::Seek { sort, keys } => seek(sort, keys, schema),
        }
    }

    pub(crate) fn seek(sort: Sort, keys: Vec<CursorKey>) -> Self {
        Self {
            kind: Kind::Seek { sort, keys },
        }
    }
}

/// Render the rows-after-a-position condition.
///
/// Always an OR-chain, never a row-value comparison like `(a, b) > (x, y)`. A
/// row value only works when every key runs the same way, so `title asc, id
/// desc` has to be written out:
///
/// ```text
/// (("title" > $1) OR ("title" = $2 AND "id" < $3))
/// ```
///
/// Writing it out always, rather than only when directions are mixed, costs a
/// few repeated binds and buys one shape to test and no driver-specific branch.
/// A positional `?` cannot point back at an earlier bind, so the repetition is
/// unavoidable on those drivers regardless.
fn seek<DB: Dialect, S: Schema>(
    sort: &Sort,
    keys: &[CursorKey],
    schema: &S,
) -> Result<QueryFragment<DB, Value>, Error> {
    let mut fragment = QueryFragment::new();
    if keys.is_empty() {
        return Ok(fragment);
    }

    let mut columns = Vec::with_capacity(keys.len());
    let mut total = false;

    for key in keys {
        let column = resolve(schema, &key.key.field)?;

        // A token is client input. One carrying text for an integer column must
        // not reach the database as a comparison between the two.
        if column.ty != key.value.ty() {
            return Err(Error::Cursor(format!(
                "holds {} for `{}`, which is {}",
                key.value.ty(),
                key.key.field,
                column.ty
            )));
        }

        total |= column.unique;
        columns.push(column);
    }

    if !total {
        return Err(Error::NotUnique(format!(
            "`{sort}` names no unique column, so a page token cannot identify a \
             row: add one with `Sort::tiebreak`, and declare it with `Table::key`"
        )));
    }

    fragment.push("(");

    for (at, key) in keys.iter().enumerate() {
        if at > 0 {
            fragment.push(" OR ");
        }
        fragment.push("(");

        for (before, earlier) in keys.iter().enumerate().take(at) {
            fragment.push(&quoted::<DB>(&columns[before].name));
            fragment.push(" = ");
            fragment.push_bind(earlier.value.clone());
            fragment.push(" AND ");
        }

        fragment.push(&quoted::<DB>(&columns[at].name));
        fragment.push(match key.key.direction {
            Direction::Asc => " > ",
            Direction::Desc => " < ",
        });
        fragment.push_bind(key.value.clone());

        fragment.push(")");
    }

    fragment.push(")");

    Ok(fragment)
}

#[cfg(test)]
mod tests {
    use sqlx::Postgres;

    use super::*;
    use crate::cursor::Cursor;
    use crate::keyset::Keyset;
    use crate::schema::{Column, ColumnType, Table};

    fn volumes() -> Table {
        Table::new()
            .key("id", ColumnType::Int)
            .column("title", ColumnType::Text)
            .add("readCount", Column::new("read_count", ColumnType::Int))
    }

    fn seek_of(order_by: &str, values: &[Value]) -> Predicate {
        let sort = Sort::parse(order_by).unwrap().tiebreak("id");
        let cursor = Cursor::new(&sort, values).unwrap();

        Keyset::new(sort, cursor).unwrap().predicate()
    }

    #[test]
    fn an_empty_predicate_renders_nothing() {
        let fragment = Predicate::empty()
            .to_fragment::<Postgres, _>(&volumes())
            .unwrap();

        assert!(Predicate::empty().is_empty());
        assert!(fragment.is_empty());
    }

    #[test]
    fn a_single_key_seeks_past_it() {
        let fragment = seek_of("", &[Value::Int(42)])
            .to_fragment::<Postgres, _>(&volumes())
            .unwrap();

        assert_eq!(fragment.preview(), r#"(("id" > ?))"#);
    }

    /// The chain, and why it exists: the directions differ, so no row-value
    /// comparison expresses this.
    #[test]
    fn mixed_directions_expand_into_a_chain() {
        let fragment = seek_of("title desc", &[Value::Text("Dune".into()), Value::Int(42)])
            .to_fragment::<Postgres, _>(&volumes())
            .unwrap();

        assert_eq!(
            fragment.preview(),
            r#"(("title" < ?) OR ("title" = ? AND "id" > ?))"#
        );
    }

    #[test]
    fn an_ascending_sort_compares_upwards() {
        let fragment = seek_of("title", &[Value::Text("Dune".into()), Value::Int(42)])
            .to_fragment::<Postgres, _>(&volumes())
            .unwrap();

        assert!(fragment.preview().starts_with(r#"(("title" > ?)"#));
    }

    #[test]
    fn a_sort_with_no_unique_column_cannot_paginate() {
        let sort = Sort::parse("title").unwrap();
        let cursor = Cursor::new(&sort, &[Value::Text("Dune".into())]).unwrap();
        let keyset = Keyset::new(sort, cursor).unwrap();

        let error = keyset
            .predicate()
            .to_fragment::<Postgres, _>(&volumes())
            .unwrap_err();

        assert!(matches!(error, Error::NotUnique(_)), "{error}");
    }

    #[test]
    fn an_unknown_field_is_rejected() {
        let error = seek_of("salary", &[Value::Int(1), Value::Int(2)])
            .to_fragment::<Postgres, _>(&volumes())
            .unwrap_err();

        assert!(matches!(error, Error::UnknownColumn(field) if field == "salary"));
    }

    /// Tokens are client input: a tampered one must not reach the database as a
    /// comparison between a string and an integer column.
    #[test]
    fn a_value_of_the_wrong_type_for_its_column_is_rejected() {
        let sort = Sort::parse("id").unwrap();
        let cursor = Cursor::new(&sort, &[Value::Text("not an id".into())]).unwrap();
        let keyset = Keyset::new(sort, cursor).unwrap();

        let error = keyset
            .predicate()
            .to_fragment::<Postgres, _>(&volumes())
            .unwrap_err();

        assert!(format!("{error}").contains("text"), "{error}");
    }
}
