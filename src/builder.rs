//! Filling a template in.

use std::fmt;

use sqlx::database::Database;
use sqlx::encode::Encode;
use sqlx::query::{Query, QueryAs, QueryScalar};
use sqlx::types::Type;
use sqlx::{Arguments, AssertSqlSafe, FromRow, IntoArguments};

use sqlx_query_core::Slot;

use crate::dialect::Dialect;
use crate::error::Error;
use crate::mapping::Mapping;
use crate::render::Render;
use crate::template::QueryTemplate;

/// A template being filled in.
///
/// # Order matters, on some drivers
///
/// Bind the skeleton's own parameters first, in skeleton order, then fill
/// slots. On PostgreSQL that is a convention; on MySQL and SQLite it is
/// enforced, because the two number placeholders differently:
///
/// * `$N` refers to the *N*th bound value, so a fragment spliced ahead of
///   `LIMIT $2` in the text does not disturb it.
/// * `?` refers to the *N*th placeholder *in the text*, so a fragment spliced
///   ahead of a `?` silently shifts it onto the wrong value.
///
/// So on a positional driver three things are errors rather than wrong answers:
/// binding after a fill, filling slots out of skeleton order, and filling a
/// slot in a skeleton whose own placeholder comes after it.
///
/// That last one is not covered by the first two. A `LIMIT ?` at the end binds
/// first and correctly, but renders after whatever the slot splices in, so it
/// would read a fragment's value. Write the value literally, or move it before
/// the first slot.
///
/// # Errors are deferred to `build`
///
/// Neither [`bind`](Self::bind) nor [`fill`](Self::fill) returns a `Result`,
/// because threading one through a builder chain costs more than it explains.
/// The first failure is kept and returned by the `build` methods.
pub struct QueryBuilder<'t, DB: Database> {
    template: &'t QueryTemplate<DB>,
    /// Resolves the request paths a producer names. Held here rather than
    /// passed to each producer, so a query has one and the chain has no `?`.
    mapping: &'t dyn Mapping,
    arguments: DB::Arguments,
    /// What has been put in each slot, indexed by the slot's position among
    /// slots rather than among pieces.
    filled: Vec<String>,
    error: Option<Error>,
    /// Whether this driver's placeholders are numbered (`$N`) or positional
    /// (`?`). See the type docs for why it matters.
    numbered: bool,
    filled_any: bool,
    last_filled: usize,
}

impl<'t, DB: Database> QueryBuilder<'t, DB> {
    pub(crate) fn new(template: &'t QueryTemplate<DB>, mapping: &'t dyn Mapping) -> Self {
        let slots = template.parts().1.len();

        Self {
            template,
            mapping,
            arguments: DB::Arguments::default(),
            filled: vec![String::new(); slots],
            error: None,
            numbered: numbered::<DB>(),
            filled_any: false,
            last_filled: 0,
        }
    }

    /// Bind one of the skeleton's own parameters.
    ///
    /// Call these in the order the skeleton numbers them, and before any
    /// [`fill`](Self::fill).
    #[must_use]
    pub fn bind<'q, T: Encode<'q, DB> + Type<DB>>(mut self, value: T) -> Self {
        if self.filled_any && !self.numbered {
            return self.fail(Error::Positional(
                "every skeleton parameter must be bound before any slot is filled".to_owned(),
            ));
        }

        if let Err(error) = self.arguments.add(value) {
            return self.fail(Error::Encode(error));
        }

        self
    }

    /// Put a fragment in a slot.
    ///
    /// Filling the same slot more than once appends, with the slot's joiner
    /// between -- which is what makes a `/* AND query.filter */` slot take a
    /// filter and a cursor condition and read correctly. An empty fragment
    /// contributes nothing at all, not even the joiner.
    #[must_use]
    pub fn fill(mut self, slot: &str, item: &impl Render<DB>) -> Self
    where
        DB: Dialect,
    {
        let Some((index, spec)) = self.find(slot) else {
            return self.unknown(slot);
        };

        let fragment = match item.to_fragment(self.mapping) {
            Ok(fragment) => fragment,
            Err(error) => return self.fail(error),
        };

        if fragment.is_empty() {
            return self;
        }

        if let Some(error) = self.check_order(index) {
            return self.fail(error);
        }

        let (segments, values) = fragment.parts();
        let mut body = String::new();

        for (segment, value) in segments.iter().zip(values) {
            body.push_str(segment);
            if let Err(error) = DB::bind(&mut self.arguments, value.clone()) {
                return self.fail(Error::Encode(error));
            }
            let _ = self.arguments.format_placeholder(&mut body);
        }
        if let Some(last) = segments.last() {
            body.push_str(last);
        }

        self.attach(index, &spec, &body);
        self
    }

    /// Build a slot by hand, for SQL no producer makes.
    ///
    /// The closure gets a [`SlotBuilder`], which binds straight into this query's
    /// argument list -- so its placeholders continue the numbering rather than
    /// restarting.
    #[must_use]
    pub fn slot(mut self, name: &str, build: impl FnOnce(&mut SlotBuilder<'_, DB>)) -> Self {
        let Some((index, spec)) = self.find(name) else {
            return self.unknown(name);
        };

        if let Some(error) = self.check_order(index) {
            return self.fail(error);
        }

        let mut body = String::new();
        build(&mut SlotBuilder {
            sql: &mut body,
            arguments: &mut self.arguments,
            error: &mut self.error,
        });

        if !body.is_empty() {
            self.attach(index, &spec, &body);
        }

        self
    }

    /// The SQL as it currently stands, for tests and tracing.
    #[must_use]
    pub fn sql(&self) -> String {
        let (texts, _) = self.template.parts();
        let mut sql = String::with_capacity(self.template.skeleton().len());

        for (text, filled) in texts.iter().zip(&self.filled) {
            sql.push_str(text);
            sql.push_str(filled);
        }

        if let Some(last) = texts.last() {
            sql.push_str(last);
        }

        sql
    }

    /// Finish, as a plain query.
    ///
    /// # Errors
    ///
    /// The first failure recorded while binding or filling: an unknown slot, a
    /// value the driver could not encode, or an ordering a positional driver
    /// cannot express.
    pub fn build(self) -> Result<Query<'t, DB, DB::Arguments>, Error>
    where
        DB::Arguments: IntoArguments<DB>,
    {
        let (sql, arguments) = self.finish()?;
        Ok(sqlx::query_with(AssertSqlSafe(sql), arguments))
    }

    /// Finish, mapping rows to `O`.
    ///
    /// # Errors
    ///
    /// As [`build`](Self::build).
    pub fn build_query_as<O>(self) -> Result<QueryAs<'t, DB, O, DB::Arguments>, Error>
    where
        O: for<'r> FromRow<'r, DB::Row>,
        DB::Arguments: IntoArguments<DB>,
    {
        let (sql, arguments) = self.finish()?;
        Ok(sqlx::query_as_with(AssertSqlSafe(sql), arguments))
    }

    /// Finish, taking the first column of each row as `O`.
    ///
    /// # Errors
    ///
    /// As [`build`](Self::build).
    pub fn build_query_scalar<O>(self) -> Result<QueryScalar<'t, DB, O, DB::Arguments>, Error>
    where
        (O,): for<'r> FromRow<'r, DB::Row>,
        DB::Arguments: IntoArguments<DB>,
    {
        let (sql, arguments) = self.finish()?;
        Ok(sqlx::query_scalar_with(AssertSqlSafe(sql), arguments))
    }

    fn finish(self) -> Result<(String, DB::Arguments), Error> {
        let sql = self.sql();
        match self.error {
            Some(error) => Err(error),
            None => Ok((sql, self.arguments)),
        }
    }

    /// Locate a slot by name, cloning its spec so the borrow ends here.
    fn find(&self, name: &str) -> Option<(usize, Slot)> {
        self.template
            .parts()
            .1
            .iter()
            .enumerate()
            .find(|(_, slot)| slot.name == name)
            .map(|(index, slot)| (index, *slot))
    }

    /// A fill out of skeleton order is only safe where placeholders are
    /// numbered; where they are positional it would shift every later `?`.
    fn check_order(&mut self, index: usize) -> Option<Error> {
        if !self.numbered && index < self.last_filled {
            return Some(Error::Positional(
                "slots must be filled in the order they appear in the skeleton".to_owned(),
            ));
        }

        self.last_filled = index;
        self.filled_any = true;
        None
    }

    /// Append `body` to a slot, with its joiner.
    ///
    /// Nothing is padded on the outside: the skeleton already has whatever
    /// whitespace surrounded the comment, and adding more would only show up in
    /// logs. Repeated fills are separated here, because nothing else will.
    fn attach(&mut self, index: usize, spec: &Slot, body: &str) {
        if !self.numbered
            && let Some(offset) = self.template.late_placeholder()
        {
            let error = Error::Positional(format!(
                "the skeleton has a placeholder at byte {offset}, after a slot: \
                 splicing in front of it would move it onto the wrong value. \
                 Put every placeholder before the first slot, or write the \
                 value literally"
            ));
            self.error.get_or_insert(error);
            return;
        }

        let slot = &mut self.filled[index];

        if !slot.is_empty() {
            slot.push(' ');
        }

        if spec.joiner.is_empty() {
            slot.push_str(body);
        } else if spec.before {
            slot.push_str(spec.joiner);
            slot.push(' ');
            slot.push_str(body);
        } else {
            slot.push_str(body);
            slot.push(' ');
            slot.push_str(spec.joiner);
        }
    }

    fn unknown(self, asked: &str) -> Self {
        let available = self.template.slots().map(str::to_owned).collect();
        self.fail(Error::UnknownSlot {
            asked: asked.to_owned(),
            available,
        })
    }

    /// Keep the first failure and carry on, so the chain stays `Result`-free.
    fn fail(mut self, error: Error) -> Self {
        if self.error.is_none() {
            self.error = Some(error);
        }
        self
    }
}

impl<DB: Database> fmt::Debug for QueryBuilder<'_, DB> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("QueryBuilder")
            .field("sql", &self.sql())
            .field("error", &self.error)
            .finish_non_exhaustive()
    }
}

/// A slot being built by hand.
///
/// Binds go straight into the query's argument list, so placeholders continue
/// its numbering. Obtained from [`QueryBuilder::slot`].
pub struct SlotBuilder<'a, DB: Database> {
    sql: &'a mut String,
    arguments: &'a mut DB::Arguments,
    error: &'a mut Option<Error>,
}

impl<DB: Database> SlotBuilder<'_, DB> {
    /// Append literal SQL. Not escaped: never pass untrusted input.
    pub fn push(&mut self, sql: impl fmt::Display) -> &mut Self {
        use fmt::Write;

        let _ = write!(self.sql, "{sql}");
        self
    }

    /// Append a bind value and its placeholder.
    pub fn push_bind<'q, T: Encode<'q, DB> + Type<DB>>(&mut self, value: T) -> &mut Self {
        match self.arguments.add(value) {
            Ok(()) => {
                let _ = self.arguments.format_placeholder(self.sql);
            }
            Err(error) => {
                if self.error.is_none() {
                    *self.error = Some(Error::Encode(error));
                }
            }
        }

        self
    }

    /// Whether anything has been pushed yet.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.sql.is_empty()
    }
}

impl<DB: Database> fmt::Debug for SlotBuilder<'_, DB> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SlotBuilder")
            .field("sql", &self.sql)
            .finish()
    }
}

/// Whether this driver numbers its placeholders.
///
/// There is no flag for this on `Database`, but `format_placeholder` has to
/// know: PostgreSQL overrides it to write `$N` and everyone else inherits the
/// default `?`. Asking a fresh argument list to write one is the cheapest way
/// to find out, and it stays correct if a driver ever changes its mind.
fn numbered<DB: Database>() -> bool {
    let mut probe = String::new();
    let _ = DB::Arguments::default().format_placeholder(&mut probe);
    probe.starts_with('$')
}

#[cfg(test)]
mod tests {
    use sqlx::{MySql, Postgres};

    use super::*;
    use crate::QueryTemplate;
    use crate::fragment::QueryFragment;
    use crate::mapping::QueryMapping;
    use crate::value::Value;

    /// Note `ORDER BY` sits *inside* the sentinel. A keyword that only makes
    /// sense with a non-empty slot has to be part of the joiner, or an unfilled
    /// slot leaves it dangling.
    const SKELETON: &str = "SELECT id FROM t WHERE tenant = $1 /* AND query.filter */ \
                            /* ORDER BY query.order */ LIMIT $2";

    /// Producers are covered where they live; these exercise the builder, so
    /// they use fragments built by hand and an empty mapping -- nothing here
    /// resolves a field.
    fn fragment<DB>(sql: &str, value: i64) -> QueryFragment<DB, Value> {
        let mut fragment = QueryFragment::new();
        fragment.push(sql).push_bind(Value::Int(value));
        fragment
    }

    fn text<DB>(sql: &str) -> QueryFragment<DB, Value> {
        let mut fragment = QueryFragment::new();
        fragment.push(sql);
        fragment
    }

    fn nothing() -> QueryMapping {
        QueryMapping::new()
    }

    /// `sqlx::Query` is not `Debug`, so `unwrap_err` is unavailable.
    fn build_error<DB: Database>(builder: QueryBuilder<'_, DB>) -> Error
    where
        DB::Arguments: IntoArguments<DB>,
    {
        match builder.build() {
            Err(error) => error,
            Ok(_) => panic!("expected the build to fail"),
        }
    }

    /// The reason splicing is safe on PostgreSQL at all: `$N` counts bound
    /// values, not placeholders in the text. `LIMIT $2` sits *after* three
    /// spliced binds and still means the second value.
    #[test]
    fn numbering_follows_bind_order_not_text_order() {
        let template = QueryTemplate::<Postgres>::parse(SKELETON).unwrap();

        let sql = template
            .builder(&nothing())
            .bind(7_i64)
            .bind(50_i64)
            .fill("filter", &fragment("reads > ", 100))
            .fill("filter", &fragment("id > ", 4711))
            .fill("order", &text("title ASC"))
            .sql();

        assert_eq!(
            sql,
            "SELECT id FROM t WHERE tenant = $1 AND reads > $3 AND id > $4 \
             ORDER BY title ASC LIMIT $2"
        );
    }

    #[test]
    fn an_empty_fragment_takes_its_joiner_with_it() {
        let template = QueryTemplate::<Postgres>::parse(SKELETON).unwrap();

        let sql = template
            .builder(&nothing())
            .bind(7_i64)
            .bind(50_i64)
            .fill("filter", &QueryFragment::<Postgres, Value>::new())
            .fill("order", &text("id ASC"))
            .sql();

        assert_eq!(
            sql,
            "SELECT id FROM t WHERE tenant = $1  ORDER BY id ASC LIMIT $2"
        );
    }

    #[test]
    fn a_trailing_joiner_follows_each_fragment() {
        let template =
            QueryTemplate::<Postgres>::parse("SELECT 1 ORDER BY /* query.order , */ id").unwrap();

        let sql = template
            .builder(&nothing())
            .fill("order", &text("title DESC"))
            .sql();

        assert_eq!(sql, "SELECT 1 ORDER BY title DESC , id");
    }

    #[test]
    fn a_hand_built_slot_continues_the_numbering() {
        let template = QueryTemplate::<Postgres>::parse(SKELETON).unwrap();

        let sql = template
            .builder(&nothing())
            .bind(7_i64)
            .bind(50_i64)
            .slot("filter", |slot| {
                slot.push("reads BETWEEN ")
                    .push_bind(1_i64)
                    .push(" AND ")
                    .push_bind(9_i64);
            })
            .sql();

        assert!(sql.contains("AND reads BETWEEN $3 AND $4"), "{sql}");
    }

    #[test]
    fn an_unknown_slot_names_the_ones_that_exist() {
        let template = QueryTemplate::<Postgres>::parse(SKELETON).unwrap();

        let error = build_error(template.builder(&nothing()).fill("predicate", &text("x")));

        let message = format!("{error}");
        assert!(message.contains("`predicate`"), "{message}");
        assert!(message.contains("`predicate`"), "{message}");
        assert!(message.contains("`order`"), "{message}");
    }

    /// On MySQL a `?` means "the next placeholder in the text", so a fragment
    /// spliced ahead of one moves it onto the wrong value. Refuse rather than
    /// bind the wrong thing.
    #[test]
    fn a_positional_driver_refuses_a_bind_after_a_fill() {
        let template = QueryTemplate::<MySql>::parse(SKELETON).unwrap();

        let error = build_error(
            template
                .builder(&nothing())
                .fill("filter", &text("reads > 1"))
                .bind(7_i64),
        );

        assert!(matches!(error, Error::Positional(_)), "{error}");
    }

    #[test]
    fn a_positional_driver_refuses_slots_filled_out_of_order() {
        let template = QueryTemplate::<MySql>::parse(SKELETON).unwrap();

        let error = build_error(
            template
                .builder(&nothing())
                .fill("order", &text("id ASC"))
                .fill("filter", &text("reads > 1")),
        );

        assert!(matches!(error, Error::Positional(_)), "{error}");
    }

    /// The hole the ordering rules alone do not close: the skeleton's own `?`
    /// is bound first and correctly, but it renders *after* whatever the slot
    /// splices in, so it ends up reading a fragment's value.
    #[test]
    fn a_positional_driver_refuses_a_placeholder_after_a_slot() {
        let template =
            QueryTemplate::<MySql>::parse("SELECT 1 /* AND query.filter */ LIMIT ?").unwrap();

        let error = build_error(
            template
                .builder(&nothing())
                .bind(50_i64)
                .fill("filter", &text("reads > 1")),
        );

        assert!(matches!(error, Error::Positional(_)), "{error}");
        assert!(format!("{error}").contains("after a slot"), "{error}");
    }

    /// Unfilled, nothing moves, so there is nothing to refuse.
    #[test]
    fn a_late_placeholder_is_fine_while_the_slot_stays_empty() {
        let template =
            QueryTemplate::<MySql>::parse("SELECT 1 /* AND query.filter */ LIMIT ?").unwrap();

        assert_eq!(
            template.builder(&nothing()).bind(50_i64).sql(),
            "SELECT 1  LIMIT ?"
        );
    }

    /// Numbered placeholders name a bound value, not a position, so the same
    /// skeleton is correct on PostgreSQL.
    #[test]
    fn a_numbered_driver_allows_a_placeholder_after_a_slot() {
        let template =
            QueryTemplate::<Postgres>::parse("SELECT 1 /* AND query.filter */ LIMIT $1").unwrap();

        let sql = template
            .builder(&nothing())
            .bind(50_i64)
            .fill("filter", &fragment("reads > ", 1))
            .sql();

        assert_eq!(sql, "SELECT 1 AND reads > $2 LIMIT $1");
    }

    /// A `?` inside a literal is not a placeholder, so it must not trip the
    /// check -- which is why the scanner records this rather than a search.
    #[test]
    fn a_question_mark_in_a_literal_is_not_a_placeholder() {
        let template =
            QueryTemplate::<MySql>::parse("SELECT 1 /* AND query.filter */ AND x = \'?\'").unwrap();

        let sql = template
            .builder(&nothing())
            .fill("filter", &text("reads > 1"))
            .sql();

        assert_eq!(sql, "SELECT 1 AND reads > 1 AND x = \'?\'");
    }

    /// The same sequence is fine where placeholders are numbered, which is why
    /// the check is driver-specific rather than a rule for everyone.
    #[test]
    fn a_numbered_driver_allows_slots_filled_out_of_order() {
        let template = QueryTemplate::<Postgres>::parse(SKELETON).unwrap();

        let sql = template
            .builder(&nothing())
            .fill("order", &text("id ASC"))
            .fill("filter", &fragment("reads > ", 1))
            .bind(7_i64)
            .sql();

        assert!(sql.contains("AND reads > $1"), "{sql}");
        assert!(sql.contains("ORDER BY id ASC"), "{sql}");
    }
}
