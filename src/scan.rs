//! A scanner over SQL text, and the two things this crate reads off it.
//!
//! Both [`placeholder_count`] and [`shift`] have to answer the same question —
//! *is this `$1` a placeholder, or is it text?* — so both walk the statement the
//! same way, stepping over the five constructs where a `$` means nothing:
//!
//! | | |
//! | --- | --- |
//! | `'…'` | a string literal, `''` escaping a quote |
//! | `"…"` | a quoted identifier, `""` escaping a quote |
//! | `-- …` | a line comment |
//! | `/* … */` | a block comment, which PostgreSQL allows to nest |
//! | `$tag$…$tag$` | a dollar-quoted string |
//!
//! Every one of those can contain a `$1`, and none of them binds anything. A
//! naive scan over `WHERE note = 'costs $1' AND id = $1` sees two placeholders
//! and renumbers the wrong one, which is a bug that survives review because the
//! SQL still parses.
//!
//! This is not a SQL parser and does not try to be. It knows where text ends,
//! which is the whole of what these two functions need.

/// Returns the highest `$N` in `sql`, or `0` when it binds nothing.
///
/// This is how many parameters a statement already has, which is what a caller
/// needs to know before splicing a fragment into it: the fragment's first
/// placeholder is this plus one.
///
/// The *highest*, not the count of occurrences. `$1` may be referenced from
/// several places and bound once, so counting occurrences would over-report;
/// and a statement is free to skip a number, in which case the driver still
/// expects that many values. The highest is the only answer that is right for
/// both.
///
/// ```
/// assert_eq!(sqlx_query::placeholder_count("SELECT * FROM t WHERE a = $1 AND b = $2"), 2);
/// assert_eq!(sqlx_query::placeholder_count("SELECT * FROM t"), 0);
/// // Text is not a placeholder, however much it looks like one.
/// assert_eq!(sqlx_query::placeholder_count("SELECT '$9' FROM t WHERE a = $1"), 1);
/// ```
#[must_use]
pub fn placeholder_count(sql: &str) -> usize {
    let mut highest = 0;

    scan(sql, |token| {
        if let Token::Placeholder(number) = token {
            highest = highest.max(number);
        }
    });

    highest
}

/// Renumbers every placeholder in `sql` by `offset`, so `$1` becomes
/// `$(1 + offset)`.
///
/// For a fragment that arrived numbered from `$1` and has to be spliced into a
/// statement that already binds parameters. **Prefer not needing it**: a
/// producer that accepts a starting offset — [`sqlx-cel`]'s `Options` and
/// [`sqlx-aip`]'s `rewrite_with`, both of which take one — emits the right
/// numbers to begin with, and then nothing has to re-read the SQL at all. Use
/// this for a fragment you were handed and cannot ask to renumber.
///
/// ```
/// // The statement binds $1 and $2 already, so the fragment starts at $3.
/// assert_eq!(sqlx_query::shift(r#""title" = $1"#, 2), r#""title" = $3"#);
/// ```
///
/// Only numbered placeholders move. A positional `?` has no number and is
/// returned untouched — see the crate docs on what that costs.
///
/// [`sqlx-cel`]: https://github.com/sqlx-contrib/sqlx-cel
/// [`sqlx-aip`]: https://github.com/sqlx-contrib/sqlx-aip
#[must_use]
pub fn shift(sql: &str, offset: usize) -> String {
    if offset == 0 {
        return sql.to_owned();
    }

    let mut shifted = String::with_capacity(sql.len());

    scan(sql, |token| match token {
        Token::Placeholder(number) => {
            shifted.push('$');
            shifted.push_str(&(number + offset).to_string());
        }
        Token::Text(text) => shifted.push_str(text),
    });

    shifted
}

/// What [`scan`] hands its visitor.
enum Token<'a> {
    /// A `$N`, already parsed. The text it came from is not passed, because
    /// every caller that wants it wants it renumbered.
    Placeholder(usize),
    /// Everything else, in runs as long as the scanner can make them.
    Text(&'a str),
}

/// Walks `sql`, calling `visit` for each placeholder and each run of text
/// between them.
///
/// The concatenation of every `Token::Text` and the source of every
/// `Token::Placeholder` is exactly `sql`, which is what lets [`shift`] rebuild
/// the statement by appending as it goes.
fn scan<'a, F>(sql: &'a str, mut visit: F)
where
    F: FnMut(Token<'a>),
{
    let bytes = sql.as_bytes();
    // The start of the current run of ordinary text, flushed whenever the
    // scanner reaches something it has to treat specially.
    let mut text = 0;
    let mut at = 0;

    while at < bytes.len() {
        // Each arm returns where the construct ends. `None` means "ordinary
        // text", and the byte is simply consumed.
        let skipped = match bytes[at] {
            b'\'' => Some(quoted(bytes, at, b'\'')),
            b'"' => Some(quoted(bytes, at, b'"')),
            b'-' if bytes.get(at + 1) == Some(&b'-') => Some(line_comment(bytes, at)),
            b'/' if bytes.get(at + 1) == Some(&b'*') => Some(block_comment(bytes, at)),
            b'$' => {
                if let Some((number, end)) = placeholder(bytes, at) {
                    if text < at {
                        visit(Token::Text(&sql[text..at]));
                    }
                    visit(Token::Placeholder(number));
                    text = end;
                    at = end;
                    continue;
                }
                // Not `$N`, so either dollar-quoting or a lone `$`.
                dollar_quoted(bytes, at)
            }
            _ => None,
        };

        at = match skipped {
            Some(end) => end,
            None => at + 1,
        };
    }

    if text < bytes.len() {
        visit(Token::Text(&sql[text..]));
    }
}

/// The end of a `'…'` or `"…"` beginning at `at`, doubled quotes included.
///
/// An unterminated literal ends at the end of the input rather than being an
/// error: this scanner reports what it can see, and a statement that does not
/// parse is the database's to complain about.
fn quoted(bytes: &[u8], at: usize, quote: u8) -> usize {
    let mut index = at + 1;

    while index < bytes.len() {
        if bytes[index] == quote {
            // A doubled quote is an escaped one, and the literal continues.
            if bytes.get(index + 1) == Some(&quote) {
                index += 2;
                continue;
            }
            return index + 1;
        }
        index += 1;
    }

    bytes.len()
}

/// The end of a `-- …` comment, including its newline.
fn line_comment(bytes: &[u8], at: usize) -> usize {
    let mut index = at + 2;

    while index < bytes.len() && bytes[index] != b'\n' {
        index += 1;
    }

    (index + 1).min(bytes.len())
}

/// The end of a `/* … */` comment, honouring PostgreSQL's nesting.
///
/// Nesting is why this cannot be a search for the next `*/`: in
/// `/* a /* b */ c */` that would stop in the middle and leave `c */` to be
/// scanned as SQL.
fn block_comment(bytes: &[u8], at: usize) -> usize {
    let mut index = at + 2;
    let mut depth = 1usize;

    while index < bytes.len() && depth > 0 {
        if bytes[index] == b'/' && bytes.get(index + 1) == Some(&b'*') {
            depth += 1;
            index += 2;
        } else if bytes[index] == b'*' && bytes.get(index + 1) == Some(&b'/') {
            depth -= 1;
            index += 2;
        } else {
            index += 1;
        }
    }

    index
}

/// The number and end of a `$N` beginning at `at`, if that is what it is.
fn placeholder(bytes: &[u8], at: usize) -> Option<(usize, usize)> {
    let mut index = at + 1;

    while index < bytes.len() && bytes[index].is_ascii_digit() {
        index += 1;
    }

    if index == at + 1 {
        return None;
    }

    // Digits only, and bounded by the length of the statement, so the parse
    // cannot fail for any reason but overflow -- at which point the statement
    // has bigger problems than this crate.
    let number = core::str::from_utf8(&bytes[at + 1..index])
        .ok()?
        .parse()
        .ok()?;

    Some((number, index))
}

/// The end of a `$tag$…$tag$` string beginning at `at`, if that is what it is.
///
/// The tag is empty (`$$…$$`) or an identifier, and the closing tag must match
/// it exactly. An unclosed one runs to the end of the input, on the same
/// reasoning as [`quoted`].
fn dollar_quoted(bytes: &[u8], at: usize) -> Option<usize> {
    let mut index = at + 1;

    while index < bytes.len() && bytes[index] != b'$' {
        let character = bytes[index];
        let valid = character.is_ascii_alphabetic()
            || character == b'_'
            // A digit is allowed inside a tag but not as its first character,
            // which is also what stops `$1` reaching here as a tag.
            || (index > at + 1 && character.is_ascii_digit());
        if !valid {
            return None;
        }
        index += 1;
    }

    if bytes.get(index) != Some(&b'$') {
        return None;
    }

    let tag = &bytes[at..=index];
    let body = index + 1;

    let close = bytes[body..]
        .windows(tag.len())
        .position(|window| window == tag);

    Some(match close {
        Some(offset) => body + offset + tag.len(),
        None => bytes.len(),
    })
}

#[cfg(test)]
mod tests {
    use super::{placeholder_count, shift};

    #[test]
    fn counts_the_highest_placeholder_rather_than_the_occurrences() {
        // $1 twice and $3 once: three values are expected, not three
        // occurrences and not two distinct numbers.
        assert_eq!(placeholder_count("a = $1 OR (b = $1 AND c = $3)"), 3);
    }

    #[test]
    fn a_statement_with_no_placeholders_binds_nothing() {
        assert_eq!(placeholder_count("SELECT 1"), 0);
        assert_eq!(placeholder_count(""), 0);
    }

    #[test]
    fn shifts_every_placeholder_and_leaves_the_rest_alone() {
        assert_eq!(
            shift(r#"("a" = $1 OR "a" = $2) AND "b" > $10"#, 2),
            r#"("a" = $3 OR "a" = $4) AND "b" > $12"#,
        );
    }

    #[test]
    fn a_zero_shift_is_the_statement_unchanged() {
        assert_eq!(shift("a = $1", 0), "a = $1");
    }

    /// The reason this is a scanner and not a regex. A `LIKE` fragment carries
    /// `'%'` literals, and a literal can carry anything at all.
    #[test]
    fn text_inside_a_string_literal_is_not_a_placeholder() {
        assert_eq!(placeholder_count("note = 'costs $9' AND id = $1"), 1);
        assert_eq!(
            shift("note = 'costs $9' AND id = $1", 4),
            "note = 'costs $9' AND id = $5",
        );
        assert_eq!(
            shift(r#""a" LIKE '%' || $1 || '%'"#, 1),
            r#""a" LIKE '%' || $2 || '%'"#,
        );
    }

    #[test]
    fn a_doubled_quote_does_not_end_the_literal() {
        // The literal is `it's $9`, so the $9 is still text.
        assert_eq!(placeholder_count("a = 'it''s $9' AND b = $1"), 1);
        assert_eq!(placeholder_count(r#""odd""name $9" = $1"#), 1);
    }

    #[test]
    fn text_inside_a_comment_is_not_a_placeholder() {
        assert_eq!(placeholder_count("a = $1 -- was $9\nAND b = $2"), 2);
        assert_eq!(placeholder_count("a = $1 /* was $9 */"), 1);
        // Nested, which is where a search for the next `*/` would stop early
        // and then scan `$9 */` as SQL.
        assert_eq!(placeholder_count("a = $1 /* /* $9 */ $9 */"), 1);
    }

    #[test]
    fn text_inside_a_dollar_quoted_string_is_not_a_placeholder() {
        assert_eq!(placeholder_count("a = $body$ $9 $body$ AND b = $1"), 1);
        assert_eq!(placeholder_count("a = $$ $9 $$ AND b = $1"), 1);
        assert_eq!(
            shift("a = $body$ $9 $body$ AND b = $1", 3),
            "a = $body$ $9 $body$ AND b = $4",
        );
    }

    /// A tag cannot start with a digit, which is what keeps `$1` a placeholder
    /// rather than the opening of a dollar-quoted string.
    #[test]
    fn a_placeholder_is_not_read_as_a_dollar_quote_tag() {
        assert_eq!(placeholder_count("a = $1$ AND b = $2"), 2);
    }

    /// Unterminated text runs to the end rather than raising: this is a
    /// scanner, and the statement is the database's to reject.
    #[test]
    fn unterminated_text_swallows_the_rest() {
        assert_eq!(placeholder_count("a = 'open $9"), 0);
        assert_eq!(placeholder_count("a = $tag$ open $9"), 0);
        assert_eq!(placeholder_count("a = $1 /* open $9"), 1);
    }

    #[test]
    fn a_lone_dollar_is_left_where_it_is() {
        assert_eq!(shift("cost = '$' || $1", 1), "cost = '$' || $2");
        assert_eq!(placeholder_count("a = $ AND b = $1"), 1);
    }

    /// Multi-byte text is copied whole rather than byte by byte, so an index
    /// landing inside a character would panic on the slice. It must not.
    #[test]
    fn text_outside_ascii_survives() {
        assert_eq!(
            shift("titel = 'Grüße $9' AND id = $1", 1),
            "titel = 'Grüße $9' AND id = $2"
        );
        assert_eq!(placeholder_count("titel = '日本語 $9' AND id = $2"), 2);
    }
}
