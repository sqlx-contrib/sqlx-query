//! Faithful port of `shiftPlaceholders` from `pgx-contrib/pgxquery`
//! (`rewriter.go`). See that file's doc comment for the full rationale;
//! summary: rewrites every *live* `$N` placeholder in a fragment of SQL
//! text to `$(N+offset)`, skipping `$N`-looking text that is actually
//! inside a single-quoted string, a double-quoted identifier, a
//! dollar-quoted body, a line comment, or a (possibly nested) block
//! comment — so a fragment can be authored with local `$1`, `$2`, ...
//! numbering independent of how many bind values already exist in the
//! base query it gets spliced into.

/// Rewrites every active `$N` placeholder in `s` to `$(N+offset)`.
pub(crate) fn shift_placeholders(s: &str, offset: usize) -> String {
    if offset == 0 {
        return s.to_owned();
    }

    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;

    while i < bytes.len() {
        let c = bytes[i];
        match c {
            b'\'' => {
                let j = skip_single_quoted(bytes, i);
                out.push_str(&s[i..j]);
                i = j;
            }
            b'"' => {
                let j = skip_double_quoted(bytes, i);
                out.push_str(&s[i..j]);
                i = j;
            }
            b'-' if bytes.get(i + 1) == Some(&b'-') => {
                let j = skip_line_comment(bytes, i);
                out.push_str(&s[i..j]);
                i = j;
            }
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                let j = skip_block_comment(bytes, i);
                out.push_str(&s[i..j]);
                i = j;
            }
            b'$' => {
                if bytes.get(i + 1).is_some_and(u8::is_ascii_digit) {
                    let mut j = i + 1;
                    while bytes.get(j).is_some_and(u8::is_ascii_digit) {
                        j += 1;
                    }
                    let n: usize = s[i + 1..j].parse().expect("scanned only ascii digits");
                    out.push('$');
                    out.push_str(&(n + offset).to_string());
                    i = j;
                    continue;
                }
                if let Some(j) = skip_dollar_quoted(bytes, i) {
                    out.push_str(&s[i..j]);
                    i = j;
                    continue;
                }
                out.push('$');
                i += 1;
            }
            _ => {
                // SAFETY-free UTF-8 handling: step by full char width so we
                // never split a multibyte character.
                let width = utf8_char_width(bytes[i]);
                out.push_str(&s[i..i + width]);
                i += width;
            }
        }
    }

    out
}

fn utf8_char_width(byte: u8) -> usize {
    match byte {
        0x00..=0x7F => 1,
        0xC0..=0xDF => 2,
        0xE0..=0xEF => 3,
        0xF0..=0xF7 => 4,
        _ => 1,
    }
}

fn skip_single_quoted(s: &[u8], i: usize) -> usize {
    let mut j = i + 1;
    while j < s.len() {
        if s[j] == b'\'' {
            if s.get(j + 1) == Some(&b'\'') {
                j += 2;
                continue;
            }
            return j + 1;
        }
        j += 1;
    }
    j
}

fn skip_double_quoted(s: &[u8], i: usize) -> usize {
    let mut j = i + 1;
    while j < s.len() {
        if s[j] == b'"' {
            if s.get(j + 1) == Some(&b'"') {
                j += 2;
                continue;
            }
            return j + 1;
        }
        j += 1;
    }
    j
}

fn skip_line_comment(s: &[u8], i: usize) -> usize {
    let mut j = i + 2;
    while j < s.len() && s[j] != b'\n' {
        j += 1;
    }
    if j < s.len() {
        j += 1;
    }
    j
}

/// Handles `/* ... */` with Postgres's nesting semantics.
fn skip_block_comment(s: &[u8], i: usize) -> usize {
    let mut j = i + 2;
    let mut depth = 1;
    while j < s.len() && depth > 0 {
        if j + 1 < s.len() && s[j] == b'/' && s[j + 1] == b'*' {
            depth += 1;
            j += 2;
        } else if j + 1 < s.len() && s[j] == b'*' && s[j + 1] == b'/' {
            depth -= 1;
            j += 2;
        } else {
            j += 1;
        }
    }
    j
}

/// Parses a dollar-quoted string starting at `i` (where `s[i]` is known to
/// be `$`). On success returns the index just past the closing tag;
/// otherwise `None`, so the caller treats the `$` as a literal character.
fn skip_dollar_quoted(s: &[u8], i: usize) -> Option<usize> {
    let mut j = i + 1;
    while j < s.len() {
        let c = s[j];
        if c == b'$' {
            break;
        }
        if c.is_ascii_alphabetic() || c == b'_' || (j > i + 1 && c.is_ascii_digit()) {
            j += 1;
            continue;
        }
        return None;
    }
    if j >= s.len() || s[j] != b'$' {
        return None;
    }
    let tag = &s[i..=j];
    let k = j + 1;
    match find_subslice(&s[k..], tag) {
        Some(idx) => Some(k + idx + tag.len()),
        // No closing tag: the rest of the string is opaque, same as
        // pgxquery's `return len(s), true`.
        None => Some(s.len()),
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
    use super::shift_placeholders;

    #[test]
    fn zero_offset_is_a_no_op() {
        assert_eq!(shift_placeholders("x = $1", 0), "x = $1");
    }

    #[test]
    fn shifts_simple_placeholders() {
        assert_eq!(
            shift_placeholders("name = $1 AND score > $2", 1),
            "name = $2 AND score > $3"
        );
    }

    #[test]
    fn shifts_placeholders_in_order_by_fragment() {
        assert_eq!(
            shift_placeholders("CASE WHEN role = $1 THEN 0 ELSE 1 END", 4),
            "CASE WHEN role = $5 THEN 0 ELSE 1 END"
        );
    }

    #[test]
    fn leaves_string_literals_and_dollar_quoted_bodies_untouched() {
        let input =
            "name = $1 AND note = 'literal $1 stays' AND body = $body$raw $1 stays$body$ AND score > $2";
        let expected =
            "name = $2 AND note = 'literal $1 stays' AND body = $body$raw $1 stays$body$ AND score > $3";
        assert_eq!(shift_placeholders(input, 1), expected);
    }

    #[test]
    fn shifts_multi_digit_placeholders() {
        assert_eq!(
            shift_placeholders("x = $1 AND y = $10 AND z = $12", 1),
            "x = $2 AND y = $11 AND z = $13"
        );
    }

    #[test]
    fn leaves_quoted_identifier_bodies_untouched() {
        assert_eq!(shift_placeholders(r#""col$1" = $1"#, 1), r#""col$1" = $2"#);
    }

    #[test]
    fn only_shifts_placeholders_outside_comments() {
        let input =
            "x = $1 -- ignore $1 here\n   AND y = $2 /* and /* nested $1 */ stays */ AND z = $3";
        let expected =
            "x = $2 -- ignore $1 here\n   AND y = $3 /* and /* nested $1 */ stays */ AND z = $4";
        assert_eq!(shift_placeholders(input, 1), expected);
    }

    #[test]
    fn treats_doubled_quote_as_escape_and_leaves_lone_dollar_alone() {
        let input = r#"note = 'it''s $1 inside' AND price > $1 AND tag <> 'A' || '$' || 'B'"#;
        let expected = r#"note = 'it''s $1 inside' AND price > $2 AND tag <> 'A' || '$' || 'B'"#;
        assert_eq!(shift_placeholders(input, 1), expected);
    }

    #[test]
    fn leaves_empty_tag_dollar_quoted_body_untouched() {
        assert_eq!(
            shift_placeholders("raw = $$keep $1 raw$$ AND id = $1", 1),
            "raw = $$keep $1 raw$$ AND id = $2"
        );
    }

    #[test]
    fn shifts_before_an_unterminated_string_literal() {
        assert_eq!(
            shift_placeholders("x = $1 AND note = 'unterminated $1", 1),
            "x = $2 AND note = 'unterminated $1"
        );
    }

    #[test]
    fn shifts_before_an_unterminated_dollar_quoted_body() {
        assert_eq!(
            shift_placeholders("x = $1 AND raw = $tag$body $1 still raw", 1),
            "x = $2 AND raw = $tag$body $1 still raw"
        );
    }

    #[test]
    fn passes_lone_dollar_and_unclosed_tag_opener_through() {
        assert_eq!(
            shift_placeholders("x = $1 AND amount > $ AND label = $abc", 1),
            "x = $2 AND amount > $ AND label = $abc"
        );
    }

    #[test]
    fn treats_doubled_double_quote_as_escape_in_identifiers() {
        assert_eq!(
            shift_placeholders(r#""weird""col $1" = $1 AND "open $1"#, 1),
            r#""weird""col $1" = $2 AND "open $1"#
        );
    }
}
