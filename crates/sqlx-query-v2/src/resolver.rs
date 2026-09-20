use std::collections::HashMap;

/// Renames the field names a fragment was parsed with to real column names,
/// against a fail-closed allow-list: any field not present as a key in
/// `columns` is an error, not passed through.
///
/// Kept separate from [`QueryFragment`](crate::QueryFragment) on purpose —
/// resolving and rendering are different steps, and not every fragment
/// needs an allow-list (`QueryFragment` impls that don't carry field names
/// simply don't implement this trait).
pub trait QueryResolver: Sized {
    type Error;

    fn resolve(self, columns: &HashMap<&str, &str>) -> Result<Self, Self::Error>;
}
