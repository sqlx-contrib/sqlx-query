//! Keyset pagination: an ordering, a position, and the condition between them.

use crate::cursor::Cursor;
use crate::error::Error;
use crate::predicate::Predicate;
use crate::sort::Sort;

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
    ///
    /// Empty on the first page, so the slot it fills -- and that slot's joiner
    /// -- disappear from the query entirely.
    #[must_use]
    pub fn predicate(&self) -> Predicate {
        if self.cursor.is_empty() {
            return Predicate::empty();
        }

        Predicate::cursor(self.cursor.keys().to_vec())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::value::Value;

    #[test]
    fn the_first_page_has_no_condition() {
        let keyset = Keyset::new(Sort::parse("title").unwrap(), Cursor::empty()).unwrap();

        assert!(keyset.predicate().is_empty());
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

    /// The ORDER BY and the seek condition come from one list, so they cannot
    /// disagree about the tiebreaker.
    #[test]
    fn the_order_by_and_the_condition_agree() {
        let sort = Sort::parse("title desc").unwrap().tiebreak("id");
        let cursor = Cursor::new(&sort, &[Value::Text("Dune".into()), Value::Int(42)]).unwrap();
        let keyset = Keyset::new(sort, cursor).unwrap();

        assert_eq!(
            keyset.sort().keys().last().unwrap().field,
            keyset.cursor().keys().last().unwrap().key.field
        );
    }
}
