//! A skeleton, parsed into literal text and slots.

use std::borrow::Cow;
use std::fmt;
use std::marker::PhantomData;

use crate::scan::Slot;
use sqlx::database::Database;

use crate::builder::QueryBuilder;
use crate::error::Error;

/// A query you already wrote, with slots where fragments go.
///
/// # Sentinels
///
/// A slot is a block comment naming `query.<name>`, optionally with a joiner on
/// one side:
///
/// ```text
/// /* AND query.filter */      joiner before each fragment
/// /* query.order , */            joiner after each fragment
/// /* query.columns */            fragments concatenated
/// ```
///
/// The name must be an identifier. One slot commonly takes fragments from
/// several sources, joined by its joiner: a `/* AND query.filter */` slot
/// holds a client's filter and a cursor's seek condition together.
///
/// A slot that is never filled, or filled only with empty fragments, emits
/// nothing at all: the comment and its joiner both disappear. That is what lets
/// a skeleton carry an `AND` it does not always need.
///
/// # The skeleton stays a real statement
///
/// Because sentinels are comments, the database ignores them. So the text you
/// wrote is a statement you can run in `psql`, `EXPLAIN`, or hand to
/// `sqlx::query!` to have it checked against a live database at compile time --
/// none of which is true of a template language with `{}` holes.
///
/// [`skeleton`](Self::skeleton) gives you that statement with the sentinels
/// removed, for exactly those uses.
pub struct QueryTemplate<DB> {
    /// Literal SQL around the slots, always one longer than `slots`.
    texts: Cow<'static, [&'static str]>,
    slots: Cow<'static, [Slot]>,
    skeleton: Cow<'static, str>,
    /// Where the first `?` that follows a slot is, if there is one.
    ///
    /// Harmless where placeholders are numbered, and fatal where they are
    /// positional -- see [`QueryBuilder`](crate::QueryBuilder). Recorded here because the
    /// scanner is the only thing that can tell a placeholder from a `?` inside
    /// a string literal.
    late_placeholder: Option<usize>,
    database: PhantomData<DB>,
}

impl<DB> QueryTemplate<DB> {
    /// Parse a skeleton.
    ///
    /// The input is `&'static str` so that every piece borrows it: parsing
    /// allocates the skeleton and the piece list, and nothing else.
    ///
    /// # Errors
    ///
    /// [`Error::Template`] if a literal or comment is unterminated, a sentinel
    /// is malformed, or a slot name is declared twice.
    pub fn parse(sql: &'static str) -> Result<Self, Error> {
        let skeleton = crate::scan::scan(sql)?;

        Ok(Self {
            texts: Cow::Owned(skeleton.texts),
            slots: Cow::Owned(skeleton.slots),
            skeleton: Cow::Owned(skeleton.sql),
            late_placeholder: skeleton.late_placeholder,
            database: PhantomData,
        })
    }

    /// The skeleton with every sentinel removed.
    ///
    /// A legal statement. Prepare it in a test to check the query you actually
    /// wrote against the database, independently of anything spliced into it.
    #[must_use]
    pub fn skeleton(&self) -> &str {
        &self.skeleton
    }

    /// Every slot this skeleton declares, in the order they appear.
    pub fn slots(&self) -> impl Iterator<Item = &str> {
        self.slots.iter().map(|slot| slot.name)
    }

    pub(crate) fn parts(&self) -> (&[&'static str], &[Slot]) {
        (&self.texts, &self.slots)
    }

    pub(crate) fn late_placeholder(&self) -> Option<usize> {
        self.late_placeholder
    }
}

impl<DB: Database> QueryTemplate<DB> {
    /// Start filling this template in.
    ///
    /// Takes no mapping: everything that fills a slot has already been
    /// resolved, which is the boundary between what a client sent and what this
    /// query will run.
    #[must_use]
    pub fn builder(&self) -> QueryBuilder<'_, DB> {
        QueryBuilder::new(self)
    }
}

impl<DB> Clone for QueryTemplate<DB> {
    fn clone(&self) -> Self {
        Self {
            texts: self.texts.clone(),
            slots: self.slots.clone(),
            skeleton: self.skeleton.clone(),
            late_placeholder: self.late_placeholder,
            database: PhantomData,
        }
    }
}

impl<DB> fmt::Debug for QueryTemplate<DB> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // `pieces` is deliberately omitted: it is the skeleton and the slots
        // interleaved, so printing it says the same thing twice and at length.
        f.debug_struct("QueryTemplate")
            .field("skeleton", &self.skeleton)
            .field("slots", &self.slots().collect::<Vec<_>>())
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    type Template = QueryTemplate<sqlx::Postgres>;

    fn slots_of(sql: &'static str) -> Vec<(String, String, bool)> {
        Template::parse(sql)
            .unwrap()
            .parts()
            .1
            .iter()
            .map(|slot| (slot.name.to_owned(), slot.joiner.to_owned(), slot.before))
            .collect()
    }

    #[test]
    fn a_joiner_may_lead_or_trail_or_be_absent() {
        assert_eq!(
            slots_of("a /* AND query.filter */ b /* query.order , */ c /* query.bare */"),
            [
                ("filter".into(), "AND".into(), true),
                ("order".into(), ",".into(), false),
                // `before` carries no meaning without a joiner to place.
                ("bare".into(), String::new(), false),
            ]
        );
    }

    #[test]
    fn the_skeleton_drops_sentinels_and_keeps_ordinary_comments() {
        let template = Template::parse(
            "SELECT id FROM t /* a note */ WHERE x = $1 /* AND query.filter */ ORDER BY id",
        )
        .unwrap();

        assert_eq!(
            template.skeleton(),
            "SELECT id FROM t /* a note */ WHERE x = $1  ORDER BY id"
        );
    }

    /// The scanner's whole reason for existing: a sentinel is only a sentinel
    /// where the database would see a comment.
    #[test]
    fn a_sentinel_inside_a_literal_is_just_text() {
        for sql in [
            "SELECT '/* AND query.filter */' FROM t",
            "SELECT \"/* AND query.filter */\" FROM t",
            "SELECT $$/* AND query.filter */$$ FROM t",
            "SELECT $tag$/* AND query.filter */$tag$ FROM t",
            "SELECT 1 -- /* AND query.filter */",
        ] {
            assert_eq!(slots_of(sql), [], "found a slot in {sql}");
        }
    }

    /// `$1` and `$$` both start with `$`, and only one of them opens a quote.
    #[test]
    fn placeholders_are_not_dollar_quotes() {
        let sql = "SELECT $1, $2 FROM t WHERE x = $3 /* AND query.filter */";
        assert_eq!(slots_of(sql).len(), 1);
    }

    #[test]
    fn doubled_quotes_escape_and_do_not_end_a_literal() {
        assert_eq!(
            slots_of("SELECT 'it''s /* query.a */' /* query.b */").len(),
            1
        );
        assert_eq!(
            slots_of(r#"SELECT "it""s /* query.a */" /* query.b */"#).len(),
            1
        );
    }

    #[test]
    fn backslash_escapes_only_inside_an_e_literal() {
        // The `\'` does not close the literal, so the sentinel stays inside it.
        assert_eq!(slots_of(r"SELECT E'\' /* query.a */' FROM t"), []);
    }

    #[test]
    fn block_comments_nest() {
        // The inner `*/` closes the inner comment, not the outer one, so the
        // sentinel is nested and therefore not a sentinel.
        assert_eq!(
            slots_of("SELECT 1 /* outer /* query.a */ still outer */"),
            []
        );
    }

    #[test]
    fn a_comment_without_the_marker_is_left_alone() {
        assert_eq!(
            slots_of("SELECT 1 /* just a note about query design */"),
            []
        );
    }

    #[test]
    fn a_duplicate_slot_is_rejected() {
        let error = Template::parse("a /* query.x */ b /* query.x */").unwrap_err();
        assert!(format!("{error}").contains("more than once"), "{error}");
    }

    /// Prose lives in comments too, so anything short of an unambiguous
    /// sentinel is left alone rather than rejected. A sentinel mistyped this
    /// far simply declares no slot, and filling it fails by name later.
    #[test]
    fn an_ambiguous_comment_is_prose_not_an_error() {
        for sql in [
            "a /* see query.rs for the parser */", // marker mid-sentence
            "a /* AND query.filter , */",          // a joiner on both sides
            "a /* query.x query.y */",             // two markers
            "a /* query.not-an-ident */",          // not an identifier
        ] {
            let template = Template::parse(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
            assert_eq!(template.slots().count(), 0, "found a slot in {sql}");
            assert_eq!(template.skeleton(), sql, "rewrote {sql}");
        }
    }

    #[test]
    fn unterminated_constructs_are_rejected() {
        for sql in [
            "SELECT 'oops",
            "SELECT \"oops",
            "SELECT $tag$oops",
            "SELECT 1 /* oops",
        ] {
            assert!(Template::parse(sql).is_err(), "accepted {sql}");
        }
    }

    #[test]
    fn slots_are_listed_in_order() {
        let template = Template::parse("a /* AND query.filter */ b /* query.order */").unwrap();
        assert_eq!(template.slots().collect::<Vec<_>>(), ["filter", "order"]);
    }
}
