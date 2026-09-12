//! Macros for [sqlx-query](https://docs.rs/sqlx-query).

use proc_macro::TokenStream;
use proc_macro2::TokenStream as TokenStream2;
use quote::quote;
use syn::{Error, LitStr, parse_macro_input};

/// Parse a skeleton at compile time.
///
/// Same scanner [`QueryTemplate::parse`] runs, run here instead -- so a
/// malformed sentinel is a compile error pointing at the string, rather than
/// something the first request finds out. The result is emitted as a `const`,
/// which is what lets a skeleton live in a `static` with no run-time parsing.
///
/// ```ignore
/// static VOLUMES: QueryTemplate<Postgres> = sql!(
///     "SELECT id, title FROM volumes
///       WHERE tenant_id = $1
///         /* AND query.predicate */
///       /* ORDER BY query.order */"
/// );
/// ```
///
/// The type parameter comes from the annotation; the macro does not know or
/// care which driver it is for.
///
/// # What it still cannot catch
///
/// Whether a slot is ever filled, and whether the name passed to `fill` matches
/// one. A skeleton declares its slots here, but the fills happen at run time
/// with names this macro never sees.
#[proc_macro]
pub fn sql(input: TokenStream) -> TokenStream {
    let literal = parse_macro_input!(input as LitStr);

    // Leaking is the cheapest way to satisfy the scanner's `&'static str`, and
    // this runs inside the compiler, which is about to exit.
    let sql: &'static str = Box::leak(literal.value().into_boxed_str());

    match sqlx_query_core::scan(sql) {
        Ok(skeleton) => skeleton_tokens(&skeleton).into(),
        Err(error) => Error::new(literal.span(), format!("invalid query template {error}"))
            .into_compile_error()
            .into(),
    }
}

fn skeleton_tokens(skeleton: &sqlx_query_core::Skeleton) -> TokenStream2 {
    let texts = skeleton.texts.iter();

    let slots = skeleton.slots.iter().map(|slot| {
        let (name, joiner, before) = (slot.name, slot.joiner, slot.before);

        quote! {
            ::sqlx_query::__private::Slot {
                name: #name,
                joiner: #joiner,
                before: #before,
            }
        }
    });

    let sql = &skeleton.sql;
    let late = if let Some(offset) = skeleton.late_placeholder {
        quote!(::core::option::Option::Some(#offset))
    } else {
        quote!(::core::option::Option::None)
    };

    quote! {
        ::sqlx_query::QueryTemplate::from_parts(
            #sql,
            &[#(#texts),*],
            &[#(#slots),*],
            #late,
        )
    }
}
