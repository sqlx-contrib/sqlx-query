//! Keyset pagination: an ordering, a position, and the condition between them.

use crate::cursor::Cursor;
use crate::dialect::{Dialect, quoted};
use crate::error::Error;
use crate::fragment::QueryFragment;
use crate::schema::Schema;
use crate::sort::{Direction, Sort, resolve};
use crate::value::Value;

/// An ordering and a position within it.
///
/// # Why not `OFFSET`
///
/// An offset counts rows the database has to produce and discard, so the last
/// page of a long list costs the most, and a row inserted or deleted between
/// pages shifts everything after it. A keyset instead asks for the rows *after
/// a specific row*, which is a range scan and is stable under concurrent
/// writes.
///
/// The price is that the ordering has to be total. See [`Sort::tiebreak`].
///
/// # The two orderings
///
/// A [`Cursor`] records the ordering it was issued under; the request supplies
/// the one it wants now. [`new`](Self::new) is where they meet, and it is the
/// only place they can be compared -- so it is where a client that changed
/// `order_by` mid-pagination is caught.
#[derive(Debug, Clone, PartialEq)]
pub struct Keyset {
    sort: Sort,
    cursor: Cursor,
}

impl Keyset {
    /// Pair an ordering with a position.
    ///
    /// Four cases, and only one of them is an error:
    ///
    /// * No cursor -- the first page. The ordering is used as given.
    /// * A cursor, and no ordering asked for. The cursor's is adopted, because
    ///   a client that sends `order_by` once and then only `page_token` has not
    ///   asked for anything different.
    /// * Both, and they agree. Used as given.
    /// * Both, and they disagree. Refused.
    ///
    /// That last case is the one worth spelling out. Reusing a token under a
    /// reversed `order_by` does not page backwards -- it flips the comparison
    /// and returns rows the client has already seen, with no error anywhere.
    /// Refusing is the only option that tells them what to fix.
    ///
    /// # Errors
    ///
    /// [`Error::Cursor`] if the token was issued under a different ordering.
    pub fn new(sort: Sort, cursor: Cursor) -> Result<Self, Error> {
        if cursor.is_empty() {
            return Ok(Self { sort, cursor });
        }

        let recorded = cursor.sort();

        if sort.is_empty() {
            return Ok(Self {
                sort: recorded,
                cursor,
            });
        }

        if sort != recorded {
            return Err(Error::Cursor(format!(
                "issued for `{recorded}`, but this request asks for `{sort}`"
            )));
        }

        Ok(Self { sort, cursor })
    }

    /// The effective ordering, tiebreaker and all.
    ///
    /// Render this into the `ORDER BY` slot. It is the same list the seek
    /// condition is built from, so the two cannot drift apart.
    #[must_use]
    pub fn sort(&self) -> &Sort {
        &self.sort
    }

    /// The position this page starts after.
    #[must_use]
    pub fn cursor(&self) -> &Cursor {
        &self.cursor
    }

    /// The condition selecting rows after that position.
    #[must_use]
    pub fn predicate(&self) -> Predicate<'_> {
        Predicate { keyset: self }
    }
}

/// The seek condition for a keyset's position.
///
/// Empty on the first page, so the slot it fills -- and that slot's joiner --
/// disappear from the query entirely.
#[derive(Debug, Clone, Copy)]
pub struct Predicate<'a> {
    keyset: &'a Keyset,
}

impl Predicate<'_> {
    /// Whether this condition would contribute anything.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.keyset.cursor.is_empty()
    }

    /// Render as a boolean expression.
    ///
    /// The shape is a chain of comparisons rather than a row-value comparison
    /// like `(a, b) > (x, y)`. A row value only works when every key runs the
    /// same way; `title asc, id desc` has to be written out:
    ///
    /// ```text
    /// (("title" > $1) OR ("title" = $2 AND "id" < $3))
    /// ```
    ///
    /// Writing it out always, rather than only when directions are mixed, costs
    /// a few repeated binds and buys one shape to test and no driver-specific
    /// branch -- a positional `?` cannot refer back to an earlier bind, so the
    /// repetition would be needed there regardless.
    ///
    /// # Errors
    ///
    /// [`Error::UnknownColumn`] for a key the schema does not expose,
    /// [`Error::NotUnique`] if no key is a unique column, and [`Error::Cursor`]
    /// if a value's type does not match its column's.
    pub fn to_fragment<DB: Dialect, S: Schema>(
        &self,
        schema: &S,
    ) -> Result<QueryFragment<DB, Value>, Error> {
        let mut fragment = QueryFragment::new();

        let keys = self.keyset.cursor.keys();
        if keys.is_empty() {
            return Ok(fragment);
        }

        let mut columns = Vec::with_capacity(keys.len());
        let mut total = false;

        for key in keys {
            let column = resolve(schema, &key.key.field)?;

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
                "`{}` names no unique column, so a page token cannot identify a \
                 row: add one with `Sort::tiebreak`, and declare it with \
                 `Table::key`",
                self.keyset.sort
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

    fn keyset(order_by: &str, values: &[Value]) -> Keyset {
        let sort = Sort::parse(order_by).unwrap().tiebreak("id");
        let cursor = Cursor::new(&sort, values).unwrap();

        Keyset::new(sort, cursor).unwrap()
    }

    #[test]
    fn the_first_page_has_no_condition() {
        let keyset = Keyset::new(Sort::parse("title").unwrap(), Cursor::empty()).unwrap();

        assert!(keyset.predicate().is_empty());
        assert!(
            keyset
                .predicate()
                .to_fragment::<Postgres, _>(&volumes())
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn a_single_key_seeks_past_it() {
        let keyset = keyset("", &[Value::Int(42)]);

        let fragment = keyset
            .predicate()
            .to_fragment::<Postgres, _>(&volumes())
            .unwrap();

        assert_eq!(fragment.preview(), r#"(("id" > ?))"#);
    }

    /// The chain, and why it exists: the directions differ, so no row-value
    /// comparison expresses this.
    #[test]
    fn mixed_directions_expand_into_a_chain() {
        let keyset = keyset("title desc", &[Value::Text("Dune".into()), Value::Int(42)]);

        let fragment = keyset
            .predicate()
            .to_fragment::<Postgres, _>(&volumes())
            .unwrap();

        assert_eq!(
            fragment.preview(),
            r#"(("title" < ?) OR ("title" = ? AND "id" > ?))"#
        );
    }

    #[test]
    fn an_ascending_sort_compares_upwards() {
        let keyset = keyset("title", &[Value::Text("Dune".into()), Value::Int(42)]);

        let fragment = keyset
            .predicate()
            .to_fragment::<Postgres, _>(&volumes())
            .unwrap();

        assert!(fragment.preview().starts_with(r#"(("title" > ?)"#));
    }

    /// A client that pages under one order and then asks for another gets an
    /// error rather than a page of rows it has already seen.
    #[test]
    fn a_token_from_a_different_order_is_refused() {
        let issued = Sort::parse("title asc").unwrap().tiebreak("id");
        let cursor = Cursor::new(&issued, &[Value::Text("Dune".into()), Value::Int(42)]).unwrap();

        let asked = Sort::parse("title desc").unwrap().tiebreak("id");
        let error = Keyset::new(asked, cursor).unwrap_err();

        let message = format!("{error}");
        assert!(message.contains("title asc"), "{message}");
        assert!(message.contains("title desc"), "{message}");
    }

    /// A client that sends `order_by` once and then only `page_token` has not
    /// asked for anything different.
    #[test]
    fn an_absent_order_by_adopts_the_token_s_own() {
        let issued = Sort::parse("title desc").unwrap().tiebreak("id");
        let cursor = Cursor::new(&issued, &[Value::Text("Dune".into()), Value::Int(42)]).unwrap();

        let keyset = Keyset::new(Sort::new(), cursor).unwrap();

        assert_eq!(keyset.sort(), &issued);
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
        let sort = Sort::parse("salary").unwrap().tiebreak("id");
        let cursor = Cursor::new(&sort, &[Value::Int(1), Value::Int(2)]).unwrap();
        let keyset = Keyset::new(sort, cursor).unwrap();

        let error = keyset
            .predicate()
            .to_fragment::<Postgres, _>(&volumes())
            .unwrap_err();

        assert!(matches!(error, Error::UnknownColumn(f) if f == "salary"),);
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

    /// The ORDER BY and the seek condition come from one list, so they cannot
    /// disagree about the tiebreaker.
    #[test]
    fn the_order_by_and_the_condition_agree() {
        let keyset = keyset("title desc", &[Value::Text("Dune".into()), Value::Int(42)]);

        let order = keyset
            .sort()
            .to_fragment::<Postgres, _>(&volumes())
            .unwrap()
            .preview();

        assert_eq!(order, r#""title" DESC, "id" ASC"#);
    }
}
