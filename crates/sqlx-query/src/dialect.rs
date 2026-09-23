/// Names the driver and hands back the [`QuerySyntax`] its SQL is read
/// with.
///
/// `DB` (`Postgres`, `MySql`, `Sqlite`, ...) is a zero-sized marker type
/// with no `Default` impl in sqlx, and [`QueryComposer`](crate::QueryComposer)
/// never holds an instance of it — `DB` only ever appears as a type
/// parameter. So, unlike the rest of this crate's public API, this is an
/// associated function rather than a `&self` method: there is no value of
/// type `DB` to call it on.
pub trait QueryDialect: sqlx::Database {
    /// Everything this crate needs to know about the dialect's text.
    fn syntax() -> QuerySyntax;
}

/// How a dialect's SQL text reads: how it spells a placeholder, and which
/// stretches of text hold none.
///
/// The two parts are separate because they answer different questions.
/// [`placeholder`](Self::placeholder) decides what the composer *produces*
/// — whether the finished statement is numbered or converted back to bare
/// `?`. [`quoting`](Self::quoting) decides what the scanner *skips*,
/// and is all the scanner needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QuerySyntax {
    /// How this dialect spells a placeholder.
    pub placeholder: PlaceholderStyle,
    /// Which stretches of its text hold no placeholders.
    pub quoting: Quoting,
}

impl QuerySyntax {
    /// How to read text whose dialect isn't known — a [`WhereClause`]'s
    /// own fragment, as opposed to a base query.
    ///
    /// A clause carries no `DB` parameter (that one-way split is the point
    /// of the crate), but its text still has to be lexed: a hand-written
    /// `WhereClause::new("note = 'costs $1 or so'")` has a `$1` that must
    /// not be shifted. With no dialect to ask, the safe choice is the SQL
    /// standard's own set — `'...'` with `''` escaping, `"..."`
    /// identifiers, `--` and non-nesting `/* ... */` — which every dialect
    /// here is a superset of.
    ///
    /// The cost is at the edges of the supersets: a hand-written fragment
    /// that dollar-quotes a `$1`, or nests a block comment around one,
    /// gets it shifted. Both are PostgreSQL-only spellings in text this
    /// crate asks you to write with plain `$N`.
    ///
    /// [`WhereClause`]: crate::WhereClause
    pub(crate) const STANDARD: Self = Self {
        placeholder: PlaceholderStyle::Number,
        quoting: Quoting {
            dollar: false,
            nested_block_comments: false,
            hash_line_comments: false,
            backtick_identifiers: false,
            bracket_identifiers: false,
            backslash_escapes: false,
        },
    };
}

/// How a placeholder names its value — the same two spellings the scanner
/// reports, seen from the dialect's side rather than the text's.
///
/// Only [`Number`](Self::Number) can be written down before its position
/// in the finished statement is known, which is why the composer numbers
/// everything internally and converts back to `?` in one final pass — see
/// [`QueryComposer::compose`](crate::QueryComposer::compose).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlaceholderStyle {
    /// `$1`, `$2`, ... — PostgreSQL.
    Number,
    /// `?` — MySQL, SQLite.
    Question,
}

impl PlaceholderStyle {
    /// Whether this dialect's placeholders carry a number of their own.
    #[must_use]
    pub fn is_number(self) -> bool {
        matches!(self, PlaceholderStyle::Number)
    }
}

/// Which stretches of a query's text hold no placeholders — string
/// literals, quoted identifiers and comments — and how each is delimited.
///
/// This is deliberately *not* a parser. The composer never needs to know
/// what a query means, only which of its `$N`/`?` occurrences are real
/// placeholders rather than characters inside a literal. Getting that
/// wrong is how a `?` inside a string turns into a bind parameter, so the
/// rules are per-dialect rather than a single permissive superset: a
/// superset would skip text one dialect quotes and another doesn't, which
/// fails in the opposite, quieter direction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "one flag per lexical rule, which is what makes each dialect's \
              `syntax()` readable as a table"
)]
pub struct Quoting {
    /// `$tag$ ... $tag$` bodies are opaque (PostgreSQL only). When false,
    /// a `$` that isn't followed by digits is just a character.
    pub dollar: bool,
    /// `/* ... /* ... */ ... */` nests (PostgreSQL only). When false, the
    /// first `*/` closes the comment.
    pub nested_block_comments: bool,
    /// `#` starts a line comment (MySQL only).
    pub hash_line_comments: bool,
    /// `` `ident` `` quotes an identifier (MySQL, SQLite).
    pub backtick_identifiers: bool,
    /// `[ident]` quotes an identifier (SQLite only).
    pub bracket_identifiers: bool,
    /// `\'` escapes inside a string literal (MySQL only, absent
    /// `NO_BACKSLASH_ESCAPES`). Doubling the quote works everywhere and is
    /// always handled.
    pub backslash_escapes: bool,
}

#[cfg(feature = "postgres")]
impl QueryDialect for sqlx::Postgres {
    fn syntax() -> QuerySyntax {
        QuerySyntax {
            placeholder: PlaceholderStyle::Number,
            quoting: Quoting {
                dollar: true,
                nested_block_comments: true,
                hash_line_comments: false,
                backtick_identifiers: false,
                bracket_identifiers: false,
                backslash_escapes: false,
            },
        }
    }
}

#[cfg(feature = "mysql")]
impl QueryDialect for sqlx::MySql {
    fn syntax() -> QuerySyntax {
        QuerySyntax {
            placeholder: PlaceholderStyle::Question,
            quoting: Quoting {
                dollar: false,
                nested_block_comments: false,
                hash_line_comments: true,
                backtick_identifiers: true,
                bracket_identifiers: false,
                backslash_escapes: true,
            },
        }
    }
}

#[cfg(feature = "sqlite")]
impl QueryDialect for sqlx::Sqlite {
    fn syntax() -> QuerySyntax {
        QuerySyntax {
            placeholder: PlaceholderStyle::Question,
            quoting: Quoting {
                dollar: false,
                nested_block_comments: false,
                hash_line_comments: false,
                backtick_identifiers: true,
                bracket_identifiers: true,
                backslash_escapes: false,
            },
        }
    }
}
