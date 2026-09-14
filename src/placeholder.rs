//! Placeholder numbering.
//!
//! Every placeholder is numbered on the way in, whatever the driver wrote it
//! as, and written back out in the driver's own form once the statement is
//! assembled. In between there is one kind of placeholder and the rewrite does
//! arithmetic on it: a fragment's `$1` becomes `$4` because three values were
//! claimed before it, and that is the whole of it.
//!
//! The number is not spelled `$4`, because the last step has to find it in the
//! rendered SQL and a string literal is allowed to contain `$4`. It is spelled
//! distinctively instead, so that the thing being searched for cannot occur by
//! accident -- the numbering is the idea, and this is only how it is written.

use std::collections::{HashMap, HashSet};
use std::ops::{ControlFlow, Range};

use sqlparser::ast::{Expr, OrderByExpr, Statement, Value, VisitMut, visit_expressions_mut};

use crate::Error;
use crate::dialect::Dialect;

/// A numbered placeholder, as it appears while the statement is being built.
const PREFIX: &str = "$__sqlxq_";
const SUFFIX: &str = "__";

/// The same, before the numbers are known: one per placeholder node, so they
/// can be told apart while their order is being worked out.
const TEMP: &str = "$__sqlxqt_";

fn numbered(n: usize) -> String {
    format!("{PREFIX}{n}{SUFFIX}")
}

fn temporary(id: usize) -> String {
    format!("{TEMP}{id}{SUFFIX}")
}

/// Renders a node so its placeholders can be read in the order they print.
///
/// Display order is the order the database sees, and it is the only authority
/// on it -- sqlparser's `Select` prints `top` before `distinct` or after it
/// depending on a runtime flag, so no traversal of the tree can stand in for
/// this.
pub(crate) trait Render {
    fn render(&self) -> String;
}

impl Render for Statement {
    fn render(&self) -> String {
        self.to_string()
    }
}

impl Render for Expr {
    fn render(&self) -> String {
        self.to_string()
    }
}

impl Render for Vec<OrderByExpr> {
    fn render(&self) -> String {
        self.iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// Finds every placeholder written with `prefix`, in the order it renders.
fn scan(sql: &str, prefix: &str) -> Vec<(Range<usize>, usize)> {
    let mut found = Vec::new();
    let mut at = 0;

    while let Some(offset) = sql[at..].find(prefix) {
        let start = at + offset;
        let digits = start + prefix.len();

        let Some(end) = sql[digits..].find(SUFFIX) else {
            at = digits;
            continue;
        };
        let Ok(n) = sql[digits..digits + end].parse::<usize>() else {
            at = digits;
            continue;
        };

        let stop = digits + end + SUFFIX.len();
        found.push((start..stop, n));
        at = stop;
    }

    found
}

/// Replaces every placeholder under `node`, handing each to `next`.
fn rewrite<V: VisitMut>(node: &mut V, mut next: impl FnMut(&str) -> String) {
    let _: ControlFlow<()> = visit_expressions_mut(node, |expr| {
        if let Expr::Value(value) = expr
            && let Value::Placeholder(text) = &mut value.value
        {
            *text = next(text);
        }
        ControlFlow::Continue(())
    });
}

/// Numbers every placeholder under `node`, continuing from `base`, and returns
/// how many values it claims.
///
/// A placeholder that came in numbered is taken at its word, so `$2` asks for
/// the second value of this fragment and `$1` twice asks for the first one
/// twice. A bare `?` asks for the next one, counted in the order it renders.
pub(crate) fn number<V: VisitMut + Render>(node: &mut V, base: usize) -> usize {
    // Tell the nodes apart first. Which value each one wants depends on where
    // it renders, and that is not known until it has been rendered.
    let mut origins = HashMap::new();
    let mut id = 0;
    rewrite(node, |text| {
        origins.insert(id, text.to_owned());
        id += 1;
        temporary(id - 1)
    });

    let mut slots: HashMap<usize, usize> = HashMap::new();
    let mut counted = 0;
    let mut named = 0;

    for (_, id) in scan(&node.render(), TEMP) {
        // `$2` and SQLite's `?2` both say which value they want; a bare `?`
        // says only that it wants one.
        let asked = origins
            .get(&id)
            .and_then(|origin| origin.strip_prefix(['$', '?']))
            .and_then(|digits| digits.parse::<usize>().ok());

        let slot = if let Some(n) = asked {
            named = named.max(n);
            n - 1
        } else {
            counted += 1;
            counted - 1
        };

        slots.insert(id, slot);
    }

    rewrite(node, |text| {
        let id = text
            .strip_prefix(TEMP)
            .and_then(|rest| rest.strip_suffix(SUFFIX))
            .and_then(|digits| digits.parse::<usize>().ok())
            .expect("every placeholder was just given a temporary number");
        numbered(base + slots[&id])
    });

    named.max(counted)
}

/// Writes the placeholders out in the driver's own form, and says which value
/// each one takes.
///
/// The returned numbers are in render order. For a driver whose placeholder
/// carries no number that is the order it binds in, so this is also what says
/// how the values have to be sent.
pub(crate) fn write<DB: Dialect>(sql: &str, arity: usize) -> Result<(String, Vec<usize>), Error> {
    let found = scan(sql, PREFIX);
    let order: Vec<usize> = found.iter().map(|(_, slot)| *slot).collect();

    let distinct: HashSet<usize> = order.iter().copied().collect();
    if distinct.len() != arity {
        return Err(Error::Orphaned);
    }

    // A placeholder with no number of its own takes a value per appearance and
    // has no way to ask for an earlier one, so one value in two places cannot
    // be written at all.
    if DB::positional() && order.len() != distinct.len() {
        return Err(Error::Positional);
    }

    let mut out = String::with_capacity(sql.len());
    let mut at = 0;

    for (span, slot) in found {
        out.push_str(&sql[at..span.start]);
        out.push_str(&DB::placeholder(slot));
        at = span.end;
    }
    out.push_str(&sql[at..]);

    Ok((out, order))
}
