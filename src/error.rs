//! The one error type this crate returns.

use std::fmt;

use sqlx::error::BoxDynError;

/// Anything that can go wrong turning a skeleton and some fragments into a
/// query.
#[derive(Debug)]
#[non_exhaustive]
pub enum Error {
    /// The skeleton could not be parsed: a malformed sentinel, an unterminated
    /// literal or comment, or the same slot declared twice.
    ///
    /// `offset` is a byte offset into the skeleton. It points at the *start* of
    /// the construct that went wrong, not at the character that proved it, so
    /// that an unterminated literal points at its opening quote.
    Template {
        /// What went wrong.
        message: String,
        /// Byte offset into the skeleton.
        offset: usize,
    },

    /// A slot was filled that the skeleton does not declare.
    ///
    /// The available names come along because the cause is nearly always a
    /// typo, and the fix is visible from the list.
    UnknownSlot {
        /// The name that was asked for.
        asked: String,
        /// Every slot the skeleton declares.
        available: Vec<String>,
    },

    /// A splice that only numbered placeholders can express was attempted on a
    /// driver that numbers them positionally.
    ///
    /// MySQL and SQLite bind `?` by its position in the text, so anything
    /// spliced ahead of a `?` moves it onto the wrong value. PostgreSQL binds
    /// `$N` by index and is unaffected, which is why this is a driver-specific
    /// error rather than a rule everywhere.
    Positional(&'static str),

    /// A driver refused to encode a bind value.
    Encode(BoxDynError),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Template { message, offset } => {
                write!(f, "invalid query template at byte {offset}: {message}")
            }
            Self::UnknownSlot { asked, available } if available.is_empty() => {
                write!(f, "no slot named `{asked}`: this template declares none")
            }
            Self::UnknownSlot { asked, available } => {
                write!(f, "no slot named `{asked}`: expected one of {}", {
                    available
                        .iter()
                        .map(|name| format!("`{name}`"))
                        .collect::<Vec<_>>()
                        .join(", ")
                })
            }
            Self::Positional(message) => {
                write!(f, "this driver uses positional `?` placeholders: {message}")
            }
            Self::Encode(error) => write!(f, "failed to encode a bind value: {error}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Encode(error) => Some(&**error),
            _ => None,
        }
    }
}

impl Error {
    /// Shorthand for the common `Template` case.
    pub(crate) fn template(message: impl Into<String>, offset: usize) -> Self {
        Self::Template {
            message: message.into(),
            offset,
        }
    }
}
