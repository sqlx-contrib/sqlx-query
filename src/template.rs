//! Parsing a skeleton into literal text and slots.

use std::borrow::Cow;
use std::fmt;
use std::marker::PhantomData;

use sqlx::database::Database;

use crate::error::Error;
use crate::splice::Splice;

/// The marker that makes a comment a sentinel rather than a comment.
const MARKER: &str = "query.";

/// One piece of a parsed skeleton.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Piece {
    /// Literal SQL, reproduced verbatim.
    Text(Cow<'static, str>),
    /// A place a fragment goes.
    Slot(SlotSpec),
}

/// What a sentinel declared.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SlotSpec {
    pub(crate) name: Cow<'static, str>,
    /// Emitted with every fragment that fills this slot, so that repeated fills
    /// read as `a AND b` rather than needing a separate separator.
    pub(crate) joiner: Cow<'static, str>,
    /// Whether the joiner precedes the fragment or follows it.
    pub(crate) before: bool,
}

/// A query you already wrote, with slots where fragments go.
///
/// # Sentinels
///
/// A slot is a block comment naming `query.<name>`, optionally with a joiner on
/// one side:
///
/// ```text
/// /* AND query.predicate */      joiner before each fragment
/// /* query.order , */            joiner after each fragment
/// /* query.columns */            fragments concatenated
/// ```
///
/// The name must be an identifier, and names the *kind of SQL* the slot holds
/// rather than whoever fills it -- one slot commonly takes fragments from
/// several sources, joined by its joiner.
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
    pieces: Cow<'static, [Piece]>,
    skeleton: Cow<'static, str>,
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
        let (pieces, skeleton) = scan(sql)?;

        Ok(Self {
            pieces: Cow::Owned(pieces),
            skeleton: Cow::Owned(skeleton),
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
        self.pieces.iter().filter_map(|piece| match piece {
            Piece::Slot(slot) => Some(&*slot.name),
            Piece::Text(_) => None,
        })
    }

    pub(crate) fn pieces(&self) -> &[Piece] {
        &self.pieces
    }
}

impl<DB: Database> QueryTemplate<DB> {
    /// Start filling this template in.
    #[must_use]
    pub fn splice(&self) -> Splice<'_, DB> {
        Splice::new(self)
    }
}

impl<DB> Clone for QueryTemplate<DB> {
    fn clone(&self) -> Self {
        Self {
            pieces: self.pieces.clone(),
            skeleton: self.skeleton.clone(),
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

// ---------------------------------------------------------------------------
// Scanner
// ---------------------------------------------------------------------------

/// Split `sql` into pieces, and build the sentinel-free skeleton alongside.
///
/// The whole point of scanning rather than searching for `/*` is that a comment
/// opener inside a string literal is not a comment, and a sentinel inside one is
/// not a sentinel. Every construct that can contain a `/*` has to be skipped:
/// quoted strings and identifiers, line comments, dollar-quoted bodies, and
/// nested block comments.
fn scan(sql: &'static str) -> Result<(Vec<Piece>, String), Error> {
    let bytes = sql.as_bytes();
    let mut pieces = Vec::new();
    let mut skeleton = String::with_capacity(sql.len());
    let mut names: Vec<Cow<'static, str>> = Vec::new();

    // Start of the literal run we are accumulating, flushed when a sentinel
    // interrupts it or the input ends.
    let mut text_start = 0;
    let mut at = 0;

    while at < bytes.len() {
        match bytes[at] {
            b'\'' => at = string_literal(sql, at)?,
            b'"' => at = delimited(sql, at, b'"')?,
            b'`' => at = delimited(sql, at, b'`')?,
            b'-' if bytes.get(at + 1) == Some(&b'-') => at = line_comment(bytes, at),
            b'$' => at = dollar_quoted(sql, at)?.unwrap_or(at + 1),
            b'/' if bytes.get(at + 1) == Some(&b'*') => {
                let (end, body) = block_comment(sql, at)?;

                if let Some(slot) = sentinel(body) {
                    if names.contains(&slot.name) {
                        return Err(Error::template(
                            format!("slot `{}` is declared more than once", slot.name),
                            at,
                        ));
                    }
                    names.push(slot.name.clone());

                    let text = &sql[text_start..at];
                    if !text.is_empty() {
                        pieces.push(Piece::Text(Cow::Borrowed(text)));
                        skeleton.push_str(text);
                    }
                    pieces.push(Piece::Slot(slot));
                    text_start = end;
                }

                at = end;
            }
            _ => at += 1,
        }
    }

    let text = &sql[text_start..];
    if !text.is_empty() {
        pieces.push(Piece::Text(Cow::Borrowed(text)));
        skeleton.push_str(text);
    }

    Ok((pieces, skeleton))
}

/// Skip a `'...'` literal, returning the index just past its closing quote.
fn string_literal(sql: &str, start: usize) -> Result<usize, Error> {
    let bytes = sql.as_bytes();

    // Postgres' `E'...'` enables backslash escapes; a plain literal does not,
    // where a backslash is an ordinary character. Doubling the quote is the
    // escape in both, and is the only one the standard defines.
    let escapes = start > 0 && matches!(bytes[start - 1], b'E' | b'e');

    let mut at = start + 1;
    while at < bytes.len() {
        match bytes[at] {
            b'\\' if escapes => at += 2,
            b'\'' if bytes.get(at + 1) == Some(&b'\'') => at += 2,
            b'\'' => return Ok(at + 1),
            _ => at += 1,
        }
    }

    Err(Error::template("unterminated string literal", start))
}

/// Skip a `"..."` or `` `...` `` identifier. Doubling the delimiter escapes it.
fn delimited(sql: &str, start: usize, delimiter: u8) -> Result<usize, Error> {
    let bytes = sql.as_bytes();

    let mut at = start + 1;
    while at < bytes.len() {
        if bytes[at] == delimiter {
            if bytes.get(at + 1) == Some(&delimiter) {
                at += 2;
            } else {
                return Ok(at + 1);
            }
        } else {
            at += 1;
        }
    }

    Err(Error::template("unterminated quoted identifier", start))
}

/// Skip a `-- ...` comment, returning the index of the newline or the end.
fn line_comment(bytes: &[u8], start: usize) -> usize {
    let mut at = start + 2;
    while at < bytes.len() && bytes[at] != b'\n' {
        at += 1;
    }
    at
}

/// Skip a `$tag$ ... $tag$` body, if this `$` opens one.
///
/// Returns `Ok(None)` when the `$` is something else -- most importantly a
/// placeholder. `$1` is not a dollar quote because a tag may not start with a
/// digit, which is exactly the rule that keeps the two apart.
fn dollar_quoted(sql: &str, start: usize) -> Result<Option<usize>, Error> {
    let bytes = sql.as_bytes();

    let mut at = start + 1;
    while at < bytes.len() && (bytes[at].is_ascii_alphanumeric() || bytes[at] == b'_') {
        if at == start + 1 && bytes[at].is_ascii_digit() {
            return Ok(None);
        }
        at += 1;
    }

    if bytes.get(at) != Some(&b'$') {
        return Ok(None);
    }

    let tag = &sql[start..=at];
    sql[at + 1..].find(tag).map_or_else(
        || {
            Err(Error::template(
                format!("unterminated dollar-quoted string opened with `{tag}`"),
                start,
            ))
        },
        |offset| Ok(Some(at + 1 + offset + tag.len())),
    )
}

/// Skip a `/* ... */` comment, returning the index past it and its body.
///
/// Postgres nests block comments, so an inner `/*` has to be counted rather
/// than the first `*/` taken as the end.
fn block_comment(sql: &'static str, start: usize) -> Result<(usize, &'static str), Error> {
    let bytes = sql.as_bytes();

    let mut depth = 0_usize;
    let mut at = start;
    while at + 1 < bytes.len() {
        if bytes[at] == b'/' && bytes[at + 1] == b'*' {
            depth += 1;
            at += 2;
        } else if bytes[at] == b'*' && bytes[at + 1] == b'/' {
            depth -= 1;
            at += 2;
            if depth == 0 {
                return Ok((at, &sql[start + 2..at - 2]));
            }
        } else {
            at += 1;
        }
    }

    Err(Error::template("unterminated block comment", start))
}

/// Read a comment body as a sentinel, or decide it is an ordinary comment.
///
/// # Ordinary comments win every tie
///
/// Sentinels are comments so that the skeleton stays a statement, which means
/// they share a namespace with prose. `/* see query.rs for the parser */` and
/// `/* outer /* query.a */ still outer */` both mention the marker and neither
/// is a slot.
///
/// So this recognises a sentinel only where the shape is unambiguous -- one
/// marker, and nothing on one side of it -- and calls anything else a comment
/// rather than an error. A sentinel mistyped past recognition is not lost: the
/// slot simply does not exist, and filling it fails with [`Error::UnknownSlot`]
/// naming every slot that does.
fn sentinel(body: &'static str) -> Option<SlotSpec> {
    let trimmed = body.trim();
    let (start, end) = marker(trimmed)?;

    let name = &trimmed[start + MARKER.len()..end];
    if !is_identifier(name) {
        return None;
    }

    let before = trimmed[..start].trim();
    let after = trimmed[end..].trim();

    // Text on both sides means the marker is in the middle of a sentence, which
    // is what prose looks like and what a sentinel never does.
    let (joiner, joiner_before) = match (before.is_empty(), after.is_empty()) {
        (true, _) => (after, false),
        (false, true) => (before, true),
        (false, false) => return None,
    };

    Some(SlotSpec {
        name: Cow::Borrowed(name),
        joiner: Cow::Borrowed(joiner),
        before: joiner_before,
    })
}

/// Locate the one `query.`-prefixed token in a comment body.
///
/// `None` when there is no marker, or more than one -- two markers is prose
/// mentioning both, not a comment declaring two slots.
fn marker(trimmed: &str) -> Option<(usize, usize)> {
    let bytes = trimmed.as_bytes();
    let mut found = None;
    let mut at = 0;

    while at < bytes.len() {
        if bytes[at].is_ascii_whitespace() {
            at += 1;
            continue;
        }

        let start = at;
        while at < bytes.len() && !bytes[at].is_ascii_whitespace() {
            at += 1;
        }

        if trimmed[start..at].starts_with(MARKER) {
            if found.is_some() {
                return None;
            }
            found = Some((start, at));
        }
    }

    found
}

fn is_identifier(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic() || first == '_')
        && chars.all(|char| char.is_ascii_alphanumeric() || char == '_')
}

#[cfg(test)]
mod tests {
    use super::*;

    type Template = QueryTemplate<sqlx::Postgres>;

    fn slots_of(sql: &'static str) -> Vec<(String, String, bool)> {
        Template::parse(sql)
            .unwrap()
            .pieces()
            .iter()
            .filter_map(|piece| match piece {
                Piece::Slot(slot) => {
                    Some((slot.name.to_string(), slot.joiner.to_string(), slot.before))
                }
                Piece::Text(_) => None,
            })
            .collect()
    }

    #[test]
    fn a_joiner_may_lead_or_trail_or_be_absent() {
        assert_eq!(
            slots_of("a /* AND query.predicate */ b /* query.order , */ c /* query.bare */"),
            [
                ("predicate".into(), "AND".into(), true),
                ("order".into(), ",".into(), false),
                // `before` carries no meaning without a joiner to place.
                ("bare".into(), String::new(), false),
            ]
        );
    }

    #[test]
    fn the_skeleton_drops_sentinels_and_keeps_ordinary_comments() {
        let template = Template::parse(
            "SELECT id FROM t /* a note */ WHERE x = $1 /* AND query.predicate */ ORDER BY id",
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
            "SELECT '/* AND query.predicate */' FROM t",
            "SELECT \"/* AND query.predicate */\" FROM t",
            "SELECT $$/* AND query.predicate */$$ FROM t",
            "SELECT $tag$/* AND query.predicate */$tag$ FROM t",
            "SELECT 1 -- /* AND query.predicate */",
        ] {
            assert_eq!(slots_of(sql), [], "found a slot in {sql}");
        }
    }

    /// `$1` and `$$` both start with `$`, and only one of them opens a quote.
    #[test]
    fn placeholders_are_not_dollar_quotes() {
        let sql = "SELECT $1, $2 FROM t WHERE x = $3 /* AND query.predicate */";
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
            "a /* AND query.predicate , */",       // a joiner on both sides
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
        let template = Template::parse("a /* AND query.predicate */ b /* query.order */").unwrap();
        assert_eq!(template.slots().collect::<Vec<_>>(), ["predicate", "order"]);
    }
}
