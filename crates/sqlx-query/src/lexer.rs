//! A single-pass scanner over SQL text that finds the two things
//! [`QueryComposer`](crate::QueryComposer) needs — placeholders and
//! slots — while skipping everything that only *looks* like
//! one: string literals, quoted identifiers, dollar-quoted bodies, line
//! comments and block comments.
//!
//! Grown out of a faithful port of `shiftPlaceholders` from
//! `pgx-contrib/pgxquery` (`rewriter.go`), which did the skipping for
//! PostgreSQL only and reported nothing back. Two things forced it wider:
//! the composer now scans the caller's *base* query (not just fragments
//! this crate generated itself), so the rules have to match the target
//! engine; and it needs to find slots here rather than with a separate
//! regex pass, so a `/* query.where */` inside a string literal isn't
//! mistaken for a real one.

use std::sync::LazyLock;

use regex::Regex;

use crate::QuerySyntax;

/// Matches a slot of the form `/* query.<name> <suffix> */`,
/// capturing the name and the trailing connective/separator text
/// (`AND`, `OR`, `,`, or nothing) so it's preserved verbatim around the
/// substituted fragment.
///
/// Anchored, and applied only to a span [`Token::scan`] has already established
/// *is* a comment — so unlike a free-running regex over the whole query,
/// a slot spelled out inside a string literal can't match.
///
/// This is the **name-first** convention used by `sqlc-gen-sqlx`'s
/// generated SQL (`/* query.where AND */`), not `pgx-contrib/pgxquery`'s
/// own connective-first convention (`/* AND query.where */`). Name-first
/// means there's no leading connective to capture, which is why this
/// pattern only has two groups.
static SLOT_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?s)^/\*\s*query\.(\w+)\b([^*]*?)\s*\*/$").unwrap());

/// Something [`Token::scan`] found that the composer acts on, named after what
/// it looks like in the text:
///
/// ```text
///   WHERE /* query.where AND */ tenant_id = $1 AND note LIKE ?
///         └───────  Slot ──────┘            └┬┘            └┬┘
///                                         Number         Question
/// ```
///
/// Everything else — literals, identifiers, comments, ordinary SQL — is
/// skipped and never reported, so a `Vec<Token>` is a sparse map of the
/// interesting offsets in the text it came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Token {
    /// A bind parameter, in whichever spelling the dialect uses.
    Placeholder(Placeholder),
    /// `/* query.where AND */` — the span a clause is spliced into,
    /// and the only comment this scanner reports rather than skips.
    Slot {
        start: usize,
        end: usize,
        name: String,
        /// The connective or separator that followed the name inside the
        /// comment, preserved so it can be re-emitted after the spliced
        /// fragment (or dropped with it, when there's nothing to splice).
        suffix: String,
    },
}

/// One bind parameter, in one of the two spellings SQL dialects use. Which
/// spelling a query uses is fixed by its dialect and never mixed —
/// numbered for PostgreSQL, bare for MySQL and SQLite.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Placeholder {
    /// `$1`, `$2`, ... — names its value by number.
    ///
    /// `number` is `N` as written, which is *not* necessarily its position
    /// among the tokens: PostgreSQL lets one value be referenced more than
    /// once, and a [`Cursor`](crate::Cursor) deliberately does.
    Number {
        start: usize,
        end: usize,
        number: usize,
    },
    /// `?` — names its value by where it sits, with no number of its own
    /// ("parameter marker", in the JDBC/ODBC sense).
    ///
    /// Reported for every dialect, acted on only by the non-positional
    /// ones: in PostgreSQL `?` is an operator (`jsonb ? text`), never a
    /// placeholder.
    Question { start: usize, end: usize },
}

/// A placeholder spelled in a way the target dialect doesn't use.
#[derive(Debug, thiserror::Error)]
pub enum PlaceholderError {
    /// A base query for a `?`-style dialect contains a numbered `$N`.
    ///
    /// Rejected rather than passed through: SQLite would read `$1` as a
    /// *named* parameter (`$` plus an identifier) and never fill it from
    /// a positional bind, and MySQL rejects it outright. Neither failure
    /// is one this crate should let through quietly, and no sqlc-generated
    /// query for these dialects produces one.
    #[error("base query uses the numbered placeholder ${number}, but this dialect's is `?`")]
    Unsupported { number: usize },
}

impl Token {
    /// The `$N` number, for a numbered placeholder only — the shape most
    /// callers want when folding over a scan.
    pub(crate) fn placeholder_number(&self) -> Option<usize> {
        match *self {
            Token::Placeholder(Placeholder::Number { number, .. }) => Some(number),
            _ => None,
        }
    }

    /// Whether this is a bare `?`, the spelling that has to be numbered
    /// before anything can be spliced past it.
    pub(crate) fn is_placeholder_question(&self) -> bool {
        matches!(self, Token::Placeholder(Placeholder::Question { .. }))
    }
}

/// Reads SQL text by a dialect's rules: what counts as a placeholder or
/// a slot, and which stretches of text hold neither.
///
/// Holds the [`QuerySyntax`] so the rules are set once, where the dialect
/// is known, rather than threaded through every call.
pub(crate) struct QueryLexer {
    syntax: QuerySyntax,
}

impl QueryLexer {
    /// A lexer for `syntax`'s dialect.
    pub(crate) fn new(syntax: QuerySyntax) -> Self {
        QueryLexer { syntax }
    }

    /// A lexer for text whose dialect isn't known — a
    /// [`WhereClause`](crate::WhereClause)'s own fragment, as opposed to a
    /// base query.
    ///
    /// A clause carries no `DB` parameter (that one-way split is the point
    /// of the crate), but its text still has to be read: a hand-written
    /// `WhereClause::new("note = 'costs $1 or so'")` has a `$1` that must
    /// not be shifted. With no dialect to ask, the safe rules are the SQL
    /// standard's, which every dialect here is a superset of — and a
    /// fragment is always numbered, whatever it ends up spliced into.
    pub(crate) fn standard() -> Self {
        QueryLexer::new(QuerySyntax::STANDARD)
    }

    /// Finds every placeholder and slot in `sql`, in textual order.
    pub(crate) fn scan(&self, sql: &str) -> Vec<Token> {
        let quoting = self.syntax.quoting;
        let bytes = sql.as_bytes();
        let mut tokens = Vec::new();
        let mut i = 0;

        while i < bytes.len() {
            match bytes[i] {
                b'\'' => i = skip_quoted(bytes, i, b'\'', quoting.backslash_escapes),
                b'"' => i = skip_quoted(bytes, i, b'"', quoting.backslash_escapes),
                b'`' if quoting.backtick_identifiers => i = skip_quoted(bytes, i, b'`', false),
                b'[' if quoting.bracket_identifiers => i = skip_bracketed(bytes, i),
                b'-' if bytes.get(i + 1) == Some(&b'-') => i = skip_line_comment(bytes, i),
                b'#' if quoting.hash_line_comments => i = skip_line_comment(bytes, i),
                b'/' if bytes.get(i + 1) == Some(&b'*') => {
                    let end = skip_block_comment(bytes, i, quoting.nested_block_comments);
                    if let Some(captures) = SLOT_RE.captures(&sql[i..end]) {
                        tokens.push(Token::Slot {
                            start: i,
                            end,
                            name: captures[1].to_owned(),
                            suffix: captures[2].to_owned(),
                        });
                    }
                    i = end;
                }
                b'$' => {
                    if bytes.get(i + 1).is_some_and(u8::is_ascii_digit) {
                        let mut end = i + 1;
                        while bytes.get(end).is_some_and(u8::is_ascii_digit) {
                            end += 1;
                        }
                        // A number too big for `usize` can't be a placeholder
                        // anyone meant; leave it as text rather than panicking.
                        if let Ok(number) = sql[i + 1..end].parse() {
                            tokens.push(Token::Placeholder(Placeholder::Number {
                                start: i,
                                end,
                                number,
                            }));
                        }
                        i = end;
                        continue;
                    }
                    if quoting.dollar {
                        if let Some(end) = skip_dollar_quoted(bytes, i) {
                            i = end;
                            continue;
                        }
                    }
                    i += 1;
                }
                b'?' => {
                    tokens.push(Token::Placeholder(Placeholder::Question {
                        start: i,
                        end: i + 1,
                    }));
                    i += 1;
                }
                _ => {
                    // Step by whole characters so a multibyte one is never split.
                    // `i` is always on a char boundary: every other arm keys off an
                    // ASCII byte, and no byte of a multibyte character is ASCII.
                    i += sql[i..].chars().next().map_or(1, char::len_utf8);
                }
            }
        }

        tokens
    }

    /// How many values these tokens' placeholders call for: the highest
    /// `$N` for a numbered dialect, the number of `?`s for one that isn't.
    ///
    /// The *highest*, not a count, because PostgreSQL lets one value be
    /// referenced repeatedly — `WHERE a = $1 OR b = $1` is one value, two
    /// references. A `?` dialect has no such thing, so counting is exact
    /// there by construction.
    ///
    /// # Errors
    ///
    /// [`PlaceholderError::Unsupported`] for a `$N` found where the
    /// dialect spells placeholders `?`.
    pub(crate) fn count_placeholders(&self, tokens: &[Token]) -> Result<usize, PlaceholderError> {
        if self.syntax.placeholder.is_number() {
            return Ok(tokens
                .iter()
                .filter_map(Token::placeholder_number)
                .max()
                .unwrap_or(0));
        }
        if let Some(number) = tokens.iter().find_map(Token::placeholder_number) {
            return Err(PlaceholderError::Unsupported { number });
        }
        Ok(tokens
            .iter()
            .filter(|token| token.is_placeholder_question())
            .count())
    }

    /// The highest `$N` in `sql`, or 0 if it has none — the offset a
    /// fragment spliced after it must be shifted by.
    ///
    /// The *highest*, not the count: a value referenced twice is still one
    /// value, and numbering that skips is numbering the caller got wrong,
    /// which [`QueryComposer::compose`](crate::QueryComposer::compose)
    /// reports rather than silently absorbing.
    pub(crate) fn max_placeholder_number(&self, sql: &str) -> usize {
        self.scan(sql)
            .iter()
            .filter_map(Token::placeholder_number)
            .max()
            .unwrap_or(0)
    }

    /// Rewrites every active `$N` in `sql` to `$(N+offset)`, so a fragment
    /// authored with local `$1`, `$2`, ... numbering can be spliced past
    /// placeholders that already exist ahead of it.
    pub(crate) fn shift_placeholder_numbers(&self, sql: &str, offset: usize) -> String {
        if offset == 0 {
            return sql.to_owned();
        }

        let mut out = String::with_capacity(sql.len());
        let mut last = 0;

        for token in self.scan(sql) {
            if let Token::Placeholder(Placeholder::Number { start, end, number }) = token {
                out.push_str(&sql[last..start]);
                out.push('$');
                out.push_str(&(number + offset).to_string());
                last = end;
            }
        }
        out.push_str(&sql[last..]);

        out
    }
}

/// Skips a `quote`-delimited run, treating a doubled quote as an escaped
/// one and (where the dialect allows it) a backslash as escaping the next
/// character. An unterminated run swallows the rest of the input, same as
/// pgxquery.
fn skip_quoted(bytes: &[u8], start: usize, quote: u8, backslash_escapes: bool) -> usize {
    let mut i = start + 1;
    while i < bytes.len() {
        let byte = bytes[i];
        if backslash_escapes && byte == b'\\' {
            i = (i + 2).min(bytes.len());
            continue;
        }
        if byte == quote {
            if bytes.get(i + 1) == Some(&quote) {
                i += 2;
                continue;
            }
            return i + 1;
        }
        i += 1;
    }
    i
}

/// Skips SQLite's `[identifier]`, which has no escape: the first `]` ends it.
fn skip_bracketed(bytes: &[u8], start: usize) -> usize {
    let mut i = start + 1;
    while i < bytes.len() {
        if bytes[i] == b']' {
            return i + 1;
        }
        i += 1;
    }
    i
}

fn skip_line_comment(bytes: &[u8], start: usize) -> usize {
    let mut i = start;
    while i < bytes.len() && bytes[i] != b'\n' {
        i += 1;
    }
    if i < bytes.len() {
        i += 1;
    }
    i
}

/// Handles `/* ... */`, with PostgreSQL's nesting semantics when `nested`
/// — MySQL and SQLite close at the first `*/` however many were opened.
fn skip_block_comment(bytes: &[u8], start: usize, nested: bool) -> usize {
    let mut i = start + 2;
    let mut depth = 1usize;
    while i < bytes.len() && depth > 0 {
        if nested && i + 1 < bytes.len() && bytes[i] == b'/' && bytes[i + 1] == b'*' {
            depth += 1;
            i += 2;
        } else if i + 1 < bytes.len() && bytes[i] == b'*' && bytes[i + 1] == b'/' {
            depth -= 1;
            i += 2;
        } else {
            i += 1;
        }
    }
    i
}

/// Parses a dollar-quoted string starting at `start` (where `bytes[start]`
/// is known to be `$`). On success returns the index just past the closing
/// tag; otherwise `None`, so the caller treats the `$` as a literal
/// character.
fn skip_dollar_quoted(bytes: &[u8], start: usize) -> Option<usize> {
    let mut i = start + 1;
    while i < bytes.len() {
        let byte = bytes[i];
        if byte == b'$' {
            break;
        }
        if byte.is_ascii_alphabetic() || byte == b'_' || (i > start + 1 && byte.is_ascii_digit()) {
            i += 1;
            continue;
        }
        return None;
    }
    if i >= bytes.len() || bytes[i] != b'$' {
        return None;
    }
    let tag = &bytes[start..=i];
    let body = i + 1;
    match find_subslice(&bytes[body..], tag) {
        Some(index) => Some(body + index + tag.len()),
        // No closing tag: the rest of the string is opaque, same as
        // pgxquery's `return len(s), true`.
        None => Some(bytes.len()),
    }
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::{QueryLexer, Token};
    use crate::QueryDialect;

    fn lexer<DB: QueryDialect>() -> QueryLexer {
        QueryLexer::new(DB::syntax())
    }

    fn shift(sql: &str, offset: usize) -> String {
        lexer::<sqlx::Postgres>().shift_placeholder_numbers(sql, offset)
    }

    fn markers<DB: QueryDialect>(sql: &str) -> usize {
        lexer::<DB>()
            .scan(sql)
            .iter()
            .filter(|token| token.is_placeholder_question())
            .count()
    }

    #[test]
    fn zero_offset_is_a_no_op() {
        assert_eq!(shift("x = $1", 0), "x = $1");
    }

    #[test]
    fn shifts_simple_placeholders() {
        assert_eq!(
            shift("name = $1 AND score > $2", 1),
            "name = $2 AND score > $3"
        );
    }

    #[test]
    fn shifts_placeholders_in_order_by_fragment() {
        assert_eq!(
            shift("CASE WHEN role = $1 THEN 0 ELSE 1 END", 4),
            "CASE WHEN role = $5 THEN 0 ELSE 1 END"
        );
    }

    #[test]
    fn leaves_string_literals_and_dollar_quoted_bodies_untouched() {
        let input =
            "name = $1 AND note = 'literal $1 stays' AND body = $body$raw $1 stays$body$ AND score > $2";
        let expected =
            "name = $2 AND note = 'literal $1 stays' AND body = $body$raw $1 stays$body$ AND score > $3";
        assert_eq!(shift(input, 1), expected);
    }

    #[test]
    fn shifts_multi_digit_placeholders() {
        assert_eq!(
            shift("x = $1 AND y = $10 AND z = $12", 1),
            "x = $2 AND y = $11 AND z = $13"
        );
    }

    #[test]
    fn leaves_quoted_identifier_bodies_untouched() {
        assert_eq!(shift(r#""col$1" = $1"#, 1), r#""col$1" = $2"#);
    }

    #[test]
    fn only_shifts_placeholders_outside_comments() {
        let input =
            "x = $1 -- ignore $1 here\n   AND y = $2 /* and /* nested $1 */ stays */ AND z = $3";
        let expected =
            "x = $2 -- ignore $1 here\n   AND y = $3 /* and /* nested $1 */ stays */ AND z = $4";
        assert_eq!(shift(input, 1), expected);
    }

    #[test]
    fn treats_doubled_quote_as_escape_and_leaves_lone_dollar_alone() {
        let input = r"note = 'it''s $1 inside' AND price > $1 AND tag <> 'A' || '$' || 'B'";
        let expected = r"note = 'it''s $1 inside' AND price > $2 AND tag <> 'A' || '$' || 'B'";
        assert_eq!(shift(input, 1), expected);
    }

    #[test]
    fn leaves_empty_tag_dollar_quoted_body_untouched() {
        assert_eq!(
            shift("raw = $$keep $1 raw$$ AND id = $1", 1),
            "raw = $$keep $1 raw$$ AND id = $2"
        );
    }

    #[test]
    fn shifts_before_an_unterminated_string_literal() {
        assert_eq!(
            shift("x = $1 AND note = 'unterminated $1", 1),
            "x = $2 AND note = 'unterminated $1"
        );
    }

    #[test]
    fn shifts_before_an_unterminated_dollar_quoted_body() {
        assert_eq!(
            shift("x = $1 AND raw = $tag$body $1 still raw", 1),
            "x = $2 AND raw = $tag$body $1 still raw"
        );
    }

    #[test]
    fn passes_lone_dollar_and_unclosed_tag_opener_through() {
        assert_eq!(
            shift("x = $1 AND amount > $ AND label = $abc", 1),
            "x = $2 AND amount > $ AND label = $abc"
        );
    }

    #[test]
    fn treats_doubled_double_quote_as_escape_in_identifiers() {
        assert_eq!(
            shift(r#""weird""col $1" = $1 AND "open $1"#, 1),
            r#""weird""col $1" = $2 AND "open $1"#
        );
    }

    #[test]
    fn max_placeholder_reads_the_highest_number_not_the_count() {
        // The cursor's shape: two values, three references.
        assert_eq!(
            lexer::<sqlx::Postgres>()
                .max_placeholder_number("(rank < $1) OR (rank = $1 AND id > $2)"),
            2
        );
        assert_eq!(
            lexer::<sqlx::Postgres>().max_placeholder_number("role = 'admin'"),
            0
        );
    }

    #[test]
    fn markers_are_found_outside_literals_and_comments() {
        let sql = "tenant = ? AND note = 'why? really' -- ? here\n AND id = ?";
        assert_eq!(markers::<sqlx::Sqlite>(sql), 2);
    }

    #[test]
    fn mysql_skips_backtick_identifiers_hash_comments_and_backslash_escapes() {
        let sql = "`weird?col` = ? AND note = 'it\\'s ? inside' # trailing ?\n AND x = ?";
        assert_eq!(markers::<sqlx::MySql>(sql), 2);
    }

    #[test]
    fn sqlite_skips_bracket_identifiers_and_does_not_nest_block_comments() {
        assert_eq!(markers::<sqlx::Sqlite>("[weird?col] = ?"), 1);
        // Non-nesting: the first `*/` closes, so the trailing `?` is live.
        assert_eq!(markers::<sqlx::Sqlite>("/* a /* b */ x = ?"), 1);
    }

    #[test]
    fn a_slot_inside_a_string_literal_is_not_a_slot() {
        let sql = "note = '/* query.where AND */' AND /* query.where AND */ TRUE";
        let slots = lexer::<sqlx::Postgres>()
            .scan(sql)
            .into_iter()
            .filter(|token| matches!(token, Token::Slot { .. }))
            .count();
        assert_eq!(slots, 1);
    }

    #[test]
    fn a_slot_carries_its_name_and_suffix() {
        let tokens = lexer::<sqlx::Postgres>().scan("/* query.where AND */");
        assert_eq!(
            tokens,
            vec![Token::Slot {
                start: 0,
                end: 21,
                // The leading space is deliberate: the suffix is
                // re-emitted verbatim after the fragment, so it carries
                // its own separation from it.
                name: "where".to_owned(),
                suffix: " AND".to_owned(),
            }]
        );
    }
}
