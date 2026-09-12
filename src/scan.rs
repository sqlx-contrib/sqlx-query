//! Reading a skeleton: literal SQL and the gaps between it.

use crate::error::Error;

/// The marker that makes a comment a sentinel rather than a comment.
const MARKER: &str = "query.";

/// A named gap a fragment goes into, as a sentinel declared it.
///
/// Borrowed from the skeleton, which is `&'static str`, so parsing allocates
/// the two lists and nothing else.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Slot {
    /// The identifier after `query.`.
    pub(crate) name: &'static str,
    /// Emitted with every fragment that fills this slot, so that repeated fills
    /// read as `a AND b` rather than needing a separate separator.
    pub(crate) joiner: &'static str,
    /// Whether the joiner precedes the fragment or follows it.
    pub(crate) before: bool,
}

/// What a skeleton turned out to be.
///
/// Text and gaps, interleaved: `texts[0]`, `slots[0]`, `texts[1]`, and so on,
/// with `texts` always one longer. The same shape a `QueryFragment` has, for
/// the same reason -- both are literal SQL around holes, and both render by
/// walking the two together.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Skeleton {
    /// The literal SQL around the slots. Always `slots.len() + 1` of them.
    pub(crate) texts: Vec<&'static str>,
    /// The gaps, in order.
    pub(crate) slots: Vec<Slot>,
    /// The same text with every sentinel removed: a legal statement.
    pub(crate) sql: String,
    /// Where the first `?` that follows a slot is, if there is one.
    ///
    /// Harmless where placeholders are numbered, and fatal where they are
    /// positional: what a slot splices in front of such a placeholder shifts it
    /// onto the wrong value.
    pub(crate) late_placeholder: Option<usize>,
}

///
/// # Errors
///
/// [`Error::Template`] if a literal or comment is unterminated, or a slot name is
/// declared twice.
pub(crate) fn scan(sql: &'static str) -> Result<Skeleton, Error> {
    let bytes = sql.as_bytes();
    let mut texts = Vec::new();
    let mut slots = Vec::new();
    let mut skeleton = String::with_capacity(sql.len());
    let mut names: Vec<&'static str> = Vec::new();

    // Start of the literal run we are accumulating, flushed when a sentinel
    // interrupts it or the input ends.
    let mut text_start = 0;
    let mut at = 0;

    // A `?` after a slot is bound out of order on positional drivers, because
    // what the slot splices in front of it shifts its position. Only the
    // scanner can tell one from a `?` inside a literal.
    let mut seen_slot = false;
    let mut late_placeholder = None;

    while at < bytes.len() {
        match bytes[at] {
            b'\'' => at = string_literal(sql, at)?,
            b'"' => at = delimited(sql, at, b'"')?,
            b'`' => at = delimited(sql, at, b'`')?,
            b'-' if bytes.get(at + 1) == Some(&b'-') => at = line_comment(bytes, at),
            b'$' => at = dollar_quoted(sql, at)?.unwrap_or(at + 1),
            b'?' => {
                if seen_slot && late_placeholder.is_none() {
                    late_placeholder = Some(at);
                }
                at += 1;
            }
            b'/' if bytes.get(at + 1) == Some(&b'*') => {
                let (end, body) = block_comment(sql, at)?;

                if let Some(slot) = sentinel(body) {
                    if names.contains(&slot.name) {
                        return Err(Error::template(
                            format!("slot `{}` is declared more than once", slot.name),
                            at,
                        ));
                    }
                    names.push(slot.name);

                    // Pushed even when empty: the two lists are read in step,
                    // so every slot needs the text that precedes it.
                    let text = &sql[text_start..at];
                    texts.push(text);
                    skeleton.push_str(text);

                    slots.push(slot);
                    seen_slot = true;
                    text_start = end;
                }

                at = end;
            }
            _ => at += 1,
        }
    }

    let text = &sql[text_start..];
    texts.push(text);
    skeleton.push_str(text);

    Ok(Skeleton {
        texts,
        slots,
        sql: skeleton,
        late_placeholder,
    })
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
/// slot simply does not exist, and filling it fails with an unknown-slot error
/// naming every slot that does.
fn sentinel(body: &'static str) -> Option<Slot> {
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

    Some(Slot {
        name,
        joiner,
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
