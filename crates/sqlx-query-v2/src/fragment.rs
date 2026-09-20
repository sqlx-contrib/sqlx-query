use crate::Value;

/// Something that renders to a standalone SQL fragment (no surrounding
/// `WHERE`/`ORDER BY` keyword, no leading or trailing connective) plus the
/// bind values it references, numbered locally as if it were the only
/// thing in the query (`$1`, `$2`, ... for positional dialects).
///
/// [`QueryComposer`](crate::QueryComposer) is the only consumer of this
/// trait: it shifts the local placeholder numbering to land in the right
/// slots of the final flat argument list and splices the rendered text
/// into a `/* query.<name> */` sentinel.
///
/// There is deliberately no separate "resolved" wrapper type — see
/// DESIGN.md's "Decisions and why" for the reasoning. Implementors that
/// also implement [`QueryResolver`](crate::QueryResolver) must be
/// resolved against the caller's column allow-list *before* being handed
/// to the composer; nothing in the type system enforces that.
pub trait QueryFragment {
    fn into_sql(self) -> (String, Vec<Value>);
}
