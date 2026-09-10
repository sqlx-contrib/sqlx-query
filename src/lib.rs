//! Splices SQL fragments into the sentinel comments of a query you already
//! wrote, for [sqlx](https://github.com/launchbadge/sqlx).
//!
//! ```
//! use sqlx_query::splice;
//!
//! const LIST_VOLUMES: &str = "\
//! SELECT * FROM volumes
//! WHERE /* query.where AND */ TRUE
//! ORDER BY /* query.order_by , */ id
//! LIMIT $1 OFFSET $2";
//!
//! let sql = splice(LIST_VOLUMES, &[
//!     ("where", Some(r#""title" = $3"#)),
//!     ("order_by", Some(r#""created_at" DESC"#)),
//! ])?;
//!
//! assert!(sql.contains(r#"WHERE "title" = $3 AND TRUE"#));
//! assert!(sql.contains(r#"ORDER BY "created_at" DESC , id"#));
//! # Ok::<_, sqlx_query::Error>(())
//! ```
//!
//! # What this crate is
//!
//! Text substitution, and a convention. A statement carries a comment where a
//! fragment may go; this puts one there. It does not build SQL, does not know
//! what a `WHERE` clause is, does not talk to a database, and has no
//! dependencies — not even sqlx.
//!
//! It is the Rust counterpart of
//! [pgxquery](https://github.com/pgx-contrib/pgxquery), which does the same job
//! at a different moment: pgx exposes a `QueryRewriter` hook, so there the
//! substitution happens as the query is sent and the caller never sees it. sqlx
//! has no such hook — `query_as` takes a string and binds positionally — so the
//! substitution has to happen where the string is built, and it is a function
//! rather than an interface.
//!
//! Where the fragments come from is not this crate's business.
//! [sqlx-cel](https://github.com/sqlx-contrib/sqlx-cel) transpiles a CEL filter
//! into one, [sqlx-aip](https://github.com/sqlx-contrib/sqlx-aip) turns a whole
//! AIP `List` request into two, and a `format!` will do. All this needs is a
//! string.
//!
//! # The sentinel
//!
//! A sentinel is a block comment naming `query.<name>`:
//!
//! ```sql
//! WHERE
//!     /* query.where AND */ TRUE
//! ORDER BY
//!     /* query.order_by , */ id  -- primary key, so the order is total
//! ```
//!
//! Whatever else is inside the comment is kept, on the side it was written:
//! `/* query.where AND */` substitutes to `<fragment> AND`, and
//! `/* query.order_by , */` to `<fragment> ,`. **The connective belongs to the
//! SQL, not to the fragment.** That is the whole trick — the author of the
//! statement decides how a fragment joins to what surrounds it, so a fragment
//! never has to know, and the same fragment can be spliced into a `WHERE` that
//! is `AND`ed and one that is `OR`ed.
//!
//! A sentinel whose fragment is [`None`] is removed, comment and connective
//! together, which is what leaves `WHERE TRUE` on an unfiltered list. A
//! statement with no fragments at all therefore runs exactly as written, which
//! is what makes the convention safe to put in generated SQL: unspliced, it is
//! a comment.
//!
//! A comment naming something not in the list — `/* query.limit */` when only
//! `where` was supplied — is left alone. This crate substitutes what it was
//! given and does not decide that a sentinel is stale.
//!
//! The reverse is an error. A fragment supplied for a sentinel the statement
//! does not carry means the predicate silently would not apply, and a dropped
//! predicate widens a result set rather than emptying it, so it fails loudly as
//! [`Error::MissingSentinel`] instead.
//!
//! # Placeholders
//!
//! A spliced fragment lands in a statement that usually binds parameters of its
//! own, and the two sets have to agree. There are two ways to arrange that, and
//! the good one costs nothing:
//!
//! **Ask the producer to start where the statement stops.** Both sqlx-cel and
//! sqlx-aip take a `param_offset`, and [`placeholder_count`] is how you know
//! what to pass:
//!
//! ```
//! # use sqlx_query::placeholder_count;
//! # const LIST_VOLUMES: &str = "SELECT * FROM volumes LIMIT $1 OFFSET $2";
//! let offset = placeholder_count(LIST_VOLUMES) + 1; // 3
//! # assert_eq!(offset, 3);
//! ```
//!
//! **Or renumber the fragment afterwards**, with [`shift`], for a fragment that
//! arrived numbered from `$1` and cannot be asked to start elsewhere. It is
//! correct, and it re-reads the SQL to do a job that need not have existed.
//!
//! Either way, the values are bound in the order the numbers say: the
//! statement's own first, the fragment's after them.
//!
//! # Positional dialects
//!
//! Everything above assumes numbered placeholders. With SQLite's or MySQL's
//! `?`, binds are matched to the *text* rather than to a number, so a fragment
//! spliced into the middle of a statement must have its values bound in the
//! middle of the list too — after the values of the placeholders before it, and
//! before those after it. [`shift`] has nothing to do there and
//! [`placeholder_count`] returns zero.
//!
//! Splicing still works; the bookkeeping moves to the caller. Splice at the end
//! of a statement, or use a numbered dialect, or count the placeholders on each
//! side of the sentinel yourself.
//!
//! # Is this safe?
//!
//! It concatenates strings into SQL, so the honest answer is: exactly as safe as
//! what you hand it. A fragment from sqlx-cel or sqlx-aip contains literals as
//! placeholders and column names from a fail-closed allow-list, and is safe to
//! splice. A fragment built by interpolating a request field is an injection,
//! and no amount of care here changes that.
//!
//! sqlx says the same thing by making you write `AssertSqlSafe` around the
//! result, which is a sentence you are asserting rather than a cast.

#![deny(missing_docs)]
#![cfg_attr(docsrs, feature(doc_cfg))]

mod scan;

pub use scan::{placeholder_count, shift};

use core::fmt;

/// The prefix that marks a comment as a sentinel.
///
/// Deliberately not configurable. The point of a convention is that a statement
/// written for one project splices in another, and pgxquery has already spelled
/// it this way.
const PREFIX: &str = "query.";

/// Substitutes `fragments` into the sentinel comments of `sql`.
///
/// Each entry is a sentinel name — the part after `query.` — and the text to
/// put there, or [`None`] to remove the sentinel. See the crate docs for the
/// convention.
///
/// ```
/// # use sqlx_query::splice;
/// let sql = splice(
///     "SELECT * FROM t WHERE /* query.where AND */ TRUE",
///     &[("where", Some("a = $1"))],
/// )?;
///
/// assert_eq!(sql, "SELECT * FROM t WHERE a = $1 AND TRUE");
/// # Ok::<_, sqlx_query::Error>(())
/// ```
///
/// # Errors
///
/// [`Error::MissingSentinel`] when a fragment is [`Some`] and `sql` has no
/// sentinel to put it in — which would otherwise drop a predicate and widen the
/// result set, silently.
pub fn splice(sql: &str, fragments: &[(&str, Option<&str>)]) -> Result<String, Error> {
    let mut spliced = String::with_capacity(sql.len());
    let mut rest = sql;
    // Which sentinels were actually found, so the check below can tell a
    // fragment that was used from one that had nowhere to go.
    let mut substituted = vec![false; fragments.len()];

    while let Some(open) = rest.find("/*") {
        let Some(length) = rest[open..].find("*/") else {
            // Unterminated, so there is no comment here to substitute into and
            // nothing further to find. The database can have its opinion.
            break;
        };
        let close = open + length + "*/".len();

        spliced.push_str(&rest[..open]);

        match sentinel(&rest[open + "/*".len()..close - "*/".len()]) {
            Some((prefix, name, suffix)) => match position(fragments, name) {
                Some(index) => {
                    substituted[index] = true;
                    if let Some(fragment) = fragments[index].1 {
                        spliced.push_str(prefix);
                        spliced.push_str(fragment);
                        spliced.push_str(suffix);
                    }
                }
                // A sentinel this call says nothing about. Left as it was:
                // it is a comment, and the statement runs with it.
                None => spliced.push_str(&rest[open..close]),
            },
            None => spliced.push_str(&rest[open..close]),
        }

        rest = &rest[close..];
    }

    spliced.push_str(rest);

    for (index, (name, fragment)) in fragments.iter().enumerate() {
        if fragment.is_some() && !substituted[index] {
            return Err(Error::MissingSentinel {
                name: (*name).to_owned(),
            });
        }
    }

    Ok(spliced)
}

/// Splits a comment body around the `query.<name>` it holds.
///
/// Returns the text either side, with the whitespace that abutted the comment
/// markers trimmed off, so `" query.where AND "` yields `("", " AND")` and the
/// substitution reads `<fragment> AND`.
///
/// The name runs to the first character that cannot be part of one, so
/// `query.where` and `query.order_by` are both found without the caller
/// declaring which names exist.
fn sentinel(body: &str) -> Option<(&str, &str, &str)> {
    let at = body.find(PREFIX)?;
    let after = at + PREFIX.len();

    let length = body[after..]
        .find(|character: char| !character.is_ascii_alphanumeric() && character != '_')
        .unwrap_or(body.len() - after);
    if length == 0 {
        return None;
    }

    Some((
        body[..at].trim_start(),
        &body[after..after + length],
        body[after + length..].trim_end(),
    ))
}

/// Finds `name` among the supplied fragments.
///
/// A linear scan: the list is two entries long in every real call, and this
/// keeps the parameter an ordinary slice rather than something the caller has
/// to build.
fn position(fragments: &[(&str, Option<&str>)], name: &str) -> Option<usize> {
    fragments
        .iter()
        .position(|(candidate, _)| *candidate == name)
}

/// Why a [`splice`] failed.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Error {
    /// A fragment was supplied for a sentinel the statement does not carry.
    ///
    /// Almost always a typo in one of the two — `/* query.wehre AND */`, or a
    /// name that was renamed on one side only. It is an error rather than a
    /// no-op because the alternative is a filter that quietly does not apply,
    /// and a query that returns *more* rows than it should is the kind of bug
    /// that reaches production.
    MissingSentinel {
        /// The name that had nowhere to go.
        name: String,
    },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingSentinel { name } => write!(
                f,
                "no /* query.{name} … */ sentinel in the statement to splice the {name} fragment into",
            ),
        }
    }
}

impl core::error::Error for Error {}

#[cfg(test)]
mod tests {
    use super::{Error, splice};

    /// The shape sqlc emits, sentinels and all.
    const LIST: &str = "SELECT *\nFROM volumes\nWHERE\n    /* query.where AND */ TRUE\nORDER BY\n    /* query.order_by , */ id\nLIMIT $1 OFFSET $2";

    fn list(where_sql: Option<&str>, order_sql: Option<&str>) -> String {
        splice(LIST, &[("where", where_sql), ("order_by", order_sql)]).unwrap()
    }

    #[test]
    fn substitutes_both_and_keeps_the_connectives_where_they_were_written() {
        let sql = list(Some(r#""title" = $3"#), Some(r#""created_at" DESC"#));

        assert!(sql.contains("WHERE\n    \"title\" = $3 AND TRUE"), "{sql}");
        assert!(
            sql.contains("ORDER BY\n    \"created_at\" DESC , id"),
            "{sql}"
        );
    }

    /// The property that makes the convention safe to generate: unspliced, the
    /// statement is the statement.
    #[test]
    fn no_fragments_leaves_the_statement_running_as_written() {
        let sql = list(None, None);

        assert!(!sql.contains("query."), "{sql}");
        assert!(sql.contains("WHERE\n     TRUE"), "{sql}");
        assert!(sql.contains("ORDER BY\n     id"), "{sql}");
    }

    #[test]
    fn one_fragment_does_not_disturb_the_others_sentinel() {
        let sql = list(Some("a = $3"), None);

        assert!(sql.contains("WHERE\n    a = $3 AND TRUE"), "{sql}");
        assert!(sql.contains("ORDER BY\n     id"), "{sql}");
    }

    /// The connective is the statement's, so the same fragment reads correctly
    /// in a query that joins it differently.
    #[test]
    fn the_statement_decides_how_a_fragment_joins() {
        let sql = splice(
            "WHERE archived /* OR query.where */",
            &[("where", Some("a = $1"))],
        )
        .unwrap();

        assert_eq!(sql, "WHERE archived OR a = $1");
    }

    #[test]
    fn a_comment_that_is_not_a_sentinel_is_left_alone() {
        let sql = splice(
            "SELECT 1 /* an ordinary comment */ WHERE /* query.where AND */ TRUE",
            &[("where", Some("a = $1"))],
        )
        .unwrap();

        assert_eq!(
            sql,
            "SELECT 1 /* an ordinary comment */ WHERE a = $1 AND TRUE"
        );
    }

    /// A sentinel the call says nothing about survives: substituting what you
    /// were not given would be deciding the statement is wrong.
    #[test]
    fn an_unmentioned_sentinel_survives() {
        let sql = splice(
            "WHERE /* query.where AND */ TRUE LIMIT /* query.limit */ 10",
            &[("where", Some("a = $1"))],
        )
        .unwrap();

        assert_eq!(sql, "WHERE a = $1 AND TRUE LIMIT /* query.limit */ 10");
    }

    /// The reverse, which is not survivable: the filter would silently not
    /// apply and the page would be wider than the caller asked for.
    #[test]
    fn a_fragment_with_nowhere_to_go_is_an_error() {
        let error = splice("SELECT * FROM t", &[("where", Some("a = $1"))]).unwrap_err();

        assert_eq!(
            error,
            Error::MissingSentinel {
                name: "where".to_owned()
            },
        );
        assert!(error.to_string().contains("query.where"), "{error}");
    }

    /// Nothing to splice, nothing to warn about.
    #[test]
    fn an_absent_fragment_with_nowhere_to_go_is_fine() {
        let sql = splice("SELECT * FROM t", &[("where", None)]).unwrap();

        assert_eq!(sql, "SELECT * FROM t");
    }

    #[test]
    fn a_sentinel_may_appear_more_than_once() {
        let sql = splice(
            "WHERE /* query.where AND */ TRUE UNION SELECT * FROM u WHERE /* query.where AND */ TRUE",
            &[("where", Some("a = $1"))],
        )
        .unwrap();

        assert_eq!(sql.matches("a = $1").count(), 2, "{sql}");
    }

    #[test]
    fn an_unterminated_comment_is_left_where_it_is() {
        let sql = splice(
            "WHERE /* query.where AND */ TRUE /* unterminated",
            &[("where", Some("a = $1"))],
        )
        .unwrap();

        assert_eq!(sql, "WHERE a = $1 AND TRUE /* unterminated");
    }

    #[test]
    fn a_bare_prefix_is_not_a_sentinel() {
        let sql = splice("SELECT 1 /* query. */", &[("where", None)]).unwrap();

        assert_eq!(sql, "SELECT 1 /* query. */");
    }

    #[test]
    fn a_statement_with_no_comments_at_all_is_returned_whole() {
        let sql = splice("SELECT 1", &[]).unwrap();

        assert_eq!(sql, "SELECT 1");
    }
}
