//! Placeholder bookkeeping.
//!
//! A rewrite moves placeholders around, and both dialect families care about
//! that in different ways, so every placeholder is replaced by a unique marker
//! on the way in and written back out once the final statement is assembled.
//!
//! The marker is what makes this a tree operation instead of a text one. Text
//! has to decide whether the `$1` it just found is a placeholder or four
//! characters inside a string literal; a marker was installed at a
//! [`Value::Placeholder`] node, so nothing else can be mistaken for one.

use std::collections::{HashMap, HashSet};
use std::ops::{ControlFlow, Range};

use sqlparser::ast::{Expr, Value, VisitMut, visit_expressions_mut};

use crate::Error;
use crate::dialect::Dialect;

// Chosen to be something no one writes by accident: `$` keeps it lexing as a
// placeholder in the dialects that have one, and the rest is not valid in an
// identifier that anybody would reach for.
const PREFIX: &str = "$__sqlxq_";
const SUFFIX: &str = "__";

fn marker(id: usize) -> String {
    format!("{PREFIX}{id}{SUFFIX}")
}

/// Replaces every placeholder under `node` with a fresh marker, recording what
/// each one used to say.
///
/// The original text is kept because it is what says which value the
/// placeholder wanted: `$2` asks for the second, `?` asks for the next one.
pub(crate) fn mark<V: VisitMut>(
    node: &mut V,
    next: &mut usize,
    origins: &mut HashMap<usize, String>,
) {
    let _: ControlFlow<()> = visit_expressions_mut(node, |expr| {
        if let Expr::Value(value) = expr
            && let Value::Placeholder(text) = &mut value.value
        {
            let id = *next;
            *next += 1;
            origins.insert(id, std::mem::replace(text, marker(id)));
        }
        ControlFlow::Continue(())
    });
}

/// Finds every marker in `sql`, in the order it renders.
///
/// Display order is the only order that matters -- it is what the database
/// will see -- and it is not necessarily the order the markers were installed
/// in, which is why this scans the rendered text rather than the tree.
fn scan(sql: &str) -> Vec<(Range<usize>, usize)> {
    let mut found = Vec::new();
    let mut at = 0;

    while let Some(offset) = sql[at..].find(PREFIX) {
        let start = at + offset;
        let digits = start + PREFIX.len();

        let Some(end) = sql[digits..].find(SUFFIX) else {
            at = digits;
            continue;
        };
        let Ok(id) = sql[digits..digits + end].parse::<usize>() else {
            at = digits;
            continue;
        };

        let stop = digits + end + SUFFIX.len();
        found.push((start..stop, id));
        at = stop;
    }

    found
}

/// Which value each marker in one fragment -- or in the base query -- asks for,
/// numbered from zero within that fragment.
pub(crate) struct Region {
    /// Marker id to the value it binds, relative to the start of the region.
    pub(crate) slots: HashMap<usize, usize>,
    /// How many values the region consumes.
    pub(crate) arity: usize,
}

/// Reads a region's placeholder numbering out of its own rendered SQL.
///
/// `$N` is taken at its word, so `$1` twice binds one value twice. `?` is
/// counted, because that is all it says.
pub(crate) fn region(rendered: &str, origins: &HashMap<usize, String>) -> Region {
    let mut slots = HashMap::new();
    let mut counted = 0;
    let mut named = 0;

    for (_, id) in scan(rendered) {
        let origin = origins.get(&id).map_or("?", String::as_str);

        let numbered = origin
            .strip_prefix('$')
            .and_then(|n| n.parse::<usize>().ok());

        let slot = if let Some(n) = numbered {
            named = named.max(n);
            n.saturating_sub(1)
        } else {
            counted += 1;
            counted - 1
        };

        slots.insert(id, slot);
    }

    Region {
        arity: named.max(counted),
        slots,
    }
}

/// Writes the final placeholders into the assembled statement, and says which
/// value each one takes.
///
/// The returned slots are in render order, which is the order a `?` dialect
/// binds in. Handing that back -- rather than insisting the rewrite produced
/// it -- is what lets a fragment land in the middle of a query whose driver
/// numbers placeholders by position: the values are bound in this order
/// instead of the order they were given.
///
/// Every slot that was claimed still has to be somewhere in the statement, or
/// a value was bound for a placeholder that no longer exists.
pub(crate) fn write<DB: Dialect>(
    sql: &str,
    slots: &HashMap<usize, usize>,
    arity: usize,
) -> Result<(String, Vec<usize>), Error> {
    let found = scan(sql);
    let order: Vec<usize> = found.iter().map(|(_, id)| slots[id]).collect();

    let present: HashSet<usize> = order.iter().copied().collect();
    if present.len() != arity {
        return Err(Error::Orphaned);
    }

    // `?` consumes a value per placeholder, so it has no way to say "the same
    // one again". `$1` twice is fine and common; `?` twice for one value is
    // not expressible, and quietly binding it twice would shift everything
    // after it.
    if DB::positional() && order.len() != present.len() {
        return Err(Error::Positional);
    }

    let mut out = String::with_capacity(sql.len());
    let mut at = 0;

    for (span, id) in found {
        out.push_str(&sql[at..span.start]);
        out.push_str(&DB::placeholder(slots[&id]));
        at = span.end;
    }
    out.push_str(&sql[at..]);

    Ok((out, order))
}
