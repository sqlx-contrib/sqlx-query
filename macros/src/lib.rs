//! Derive macros for [sqlx-query](https://docs.rs/sqlx-query).

use proc_macro::TokenStream;
use proc_macro2::TokenStream as TokenStream2;
use quote::quote;
use syn::spanned::Spanned as _;
use syn::{Attribute, Data, DeriveInput, Error, Fields, Ident, LitStr, Type, parse_macro_input};

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
    let pieces = skeleton.pieces.iter().map(|piece| match piece {
        sqlx_query_core::Piece::Text(text) => quote! {
            ::sqlx_query::__private::Piece::Text(#text)
        },
        sqlx_query_core::Piece::Slot(slot) => {
            let (name, joiner, before) = (slot.name, slot.joiner, slot.before);

            quote! {
                ::sqlx_query::__private::Piece::Slot(::sqlx_query::__private::Slot {
                    name: #name,
                    joiner: #joiner,
                    before: #before,
                })
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
        ::sqlx_query::QueryTemplate::from_parts(#sql, &[#(#pieces),*], #late)
    }
}

/// Declare a table's allow-list on the struct that describes it.
///
/// Generates an inherent `schema()` returning a `&'static Table`, so the
/// mapping is written once and cannot drift from the fields it describes.
///
/// ```ignore
/// #[derive(Schema)]
/// #[schema(rename_all = "camelCase")]
/// struct Volume {
///     #[schema(key)]
///     id: i64,
///     title: String,
///     read_count: i64,                       // exposed as `readCount`
///     #[schema(column = "author_name")]
///     author: String,
///     #[schema(skip)]
///     internal_note: String,
/// }
///
/// let schema = Volume::schema();
/// ```
///
/// # Field options
///
/// * `key` -- the column is unique, so a sort ending here is total and a page
///   token can identify an exact row.
/// * `rename = "..."` -- the request-facing name, overriding `rename_all`.
/// * `column = "..."` -- the database column, if it differs from the field.
/// * `ty = "int"` -- override the inferred column type.
/// * `skip` -- leave the field out, so no request may name it.
///
/// # Why this is not the whole schema
///
/// A schema is an allow-list: what an API chooses to expose, under what public
/// name. That is deliberately narrower than what the table holds, which is why
/// it is declared rather than reflected -- generating it from every column is
/// exactly what fail-closed exists to prevent.
#[proc_macro_derive(Schema, attributes(schema))]
pub fn derive_schema(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);

    expand(&input)
        .unwrap_or_else(Error::into_compile_error)
        .into()
}

fn expand(input: &DeriveInput) -> syn::Result<TokenStream2> {
    if !input.generics.params.is_empty() {
        return Err(Error::new(
            input.generics.span(),
            "`Schema` cannot be derived for a generic type: the schema is held \
             in a `static`, which every instantiation would share",
        ));
    }

    let Data::Struct(data) = &input.data else {
        return Err(Error::new(
            input.span(),
            "`Schema` can only be derived for a struct with named fields",
        ));
    };

    let Fields::Named(fields) = &data.fields else {
        return Err(Error::new(
            data.fields.span(),
            "`Schema` can only be derived for a struct with named fields",
        ));
    };

    let case = rename_all(&input.attrs)?;
    let mut columns = Vec::new();

    for field in &fields.named {
        let options = FieldOptions::parse(&field.attrs)?;
        if options.skip {
            continue;
        }

        let name = field
            .ident
            .as_ref()
            .ok_or_else(|| Error::new(field.span(), "every field needs a name"))?
            .to_string();

        let exposed = options.rename.unwrap_or_else(|| case.apply(&name));
        let column = options.column.unwrap_or(name);

        let kind = match options.kind {
            Some(kind) => kind,
            None => infer(&field.ty).ok_or_else(|| {
                Error::new(
                    field.ty.span(),
                    "cannot tell what column type this is: name it with \
                     `#[schema(ty = \"...\")]`, or leave the field out with \
                     `#[schema(skip)]`",
                )
            })?,
        };

        let kind = Ident::new(kind, field.ty.span());
        let constructor = if options.key {
            quote!(key)
        } else {
            quote!(new)
        };

        columns.push(quote! {
            .add(
                #exposed,
                ::sqlx_query::Column::#constructor(#column, ::sqlx_query::ColumnType::#kind),
            )
        });
    }

    let name = &input.ident;

    Ok(quote! {
        impl #name {
            /// The allow-list this type declares.
            ///
            /// Built once and shared: a request path not named here is rejected
            /// rather than passed through.
            #[must_use]
            pub fn schema() -> &'static ::sqlx_query::Table {
                static SCHEMA: ::std::sync::OnceLock<::sqlx_query::Table> =
                    ::std::sync::OnceLock::new();

                SCHEMA.get_or_init(|| ::sqlx_query::Table::new() #(#columns)*)
            }
        }
    })
}

/// How a field name becomes a request-facing name.
#[derive(Clone, Copy, Default)]
enum Case {
    /// Leave it as the field is spelled.
    #[default]
    AsIs,
    Camel,
    Pascal,
}

impl Case {
    fn apply(self, field: &str) -> String {
        match self {
            Self::AsIs => field.to_owned(),
            Self::Camel => convert(field, false),
            Self::Pascal => convert(field, true),
        }
    }
}

/// Turn `read_count` into `readCount` or `ReadCount`.
fn convert(field: &str, capitalise_first: bool) -> String {
    let mut out = String::with_capacity(field.len());
    let mut capitalise = capitalise_first;

    for character in field.chars() {
        if character == '_' {
            capitalise = true;
        } else if capitalise {
            out.extend(character.to_uppercase());
            capitalise = false;
        } else {
            out.push(character);
        }
    }

    out
}

fn rename_all(attrs: &[Attribute]) -> syn::Result<Case> {
    let mut case = Case::default();

    for attr in attrs.iter().filter(|attr| attr.path().is_ident("schema")) {
        attr.parse_nested_meta(|meta| {
            if !meta.path.is_ident("rename_all") {
                return Err(meta.error("unknown option: the struct takes `rename_all`"));
            }

            let value: LitStr = meta.value()?.parse()?;
            case = match value.value().as_str() {
                "camelCase" => Case::Camel,
                "PascalCase" => Case::Pascal,
                "snake_case" => Case::AsIs,
                other => {
                    return Err(Error::new(
                        value.span(),
                        format!(
                            "`{other}` is not a case: expected `camelCase`, \
                             `PascalCase` or `snake_case`"
                        ),
                    ));
                }
            };

            Ok(())
        })?;
    }

    Ok(case)
}

#[derive(Default)]
struct FieldOptions {
    rename: Option<String>,
    column: Option<String>,
    kind: Option<&'static str>,
    key: bool,
    skip: bool,
}

impl FieldOptions {
    fn parse(attrs: &[Attribute]) -> syn::Result<Self> {
        let mut options = Self::default();

        for attr in attrs.iter().filter(|attr| attr.path().is_ident("schema")) {
            attr.parse_nested_meta(|meta| {
                if meta.path.is_ident("key") {
                    options.key = true;
                } else if meta.path.is_ident("skip") {
                    options.skip = true;
                } else if meta.path.is_ident("rename") {
                    options.rename = Some(meta.value()?.parse::<LitStr>()?.value());
                } else if meta.path.is_ident("column") {
                    options.column = Some(meta.value()?.parse::<LitStr>()?.value());
                } else if meta.path.is_ident("ty") {
                    let value: LitStr = meta.value()?.parse()?;
                    options.kind = Some(match value.value().as_str() {
                        "bool" => "Bool",
                        "int" => "Int",
                        "float" => "Float",
                        "text" => "Text",
                        "bytes" => "Bytes",
                        "timestamp" => "Timestamp",
                        other => {
                            return Err(Error::new(
                                value.span(),
                                format!(
                                    "`{other}` is not a column type: expected `bool`, \
                                     `int`, `float`, `text`, `bytes` or `timestamp`"
                                ),
                            ));
                        }
                    });
                } else {
                    return Err(meta.error(
                        "unknown option: a field takes `key`, `skip`, `rename`, \
                         `column` or `ty`",
                    ));
                }

                Ok(())
            })?;
        }

        Ok(options)
    }
}

/// Guess the column type from the Rust one.
///
/// Deliberately conservative: anything not listed is a compile error naming the
/// escape hatch, rather than a guess that would surface as a type mismatch at
/// request time.
fn infer(ty: &Type) -> Option<&'static str> {
    match ty {
        Type::Reference(reference) => infer(&reference.elem),
        Type::Path(path) => {
            let segment = path.path.segments.last()?;

            match segment.ident.to_string().as_str() {
                "bool" => Some("Bool"),
                "i8" | "i16" | "i32" | "i64" | "u8" | "u16" | "u32" => Some("Int"),
                "f32" | "f64" => Some("Float"),
                "String" | "str" => Some("Text"),
                "DateTime" | "NaiveDateTime" => Some("Timestamp"),
                // `Vec<u8>` is bytes; any other `Vec` is an array this crate
                // has no comparison for.
                "Vec" => match inner(segment)? {
                    Type::Path(inner) if inner.path.is_ident("u8") => Some("Bytes"),
                    _ => None,
                },
                // A nullable column is still that column's type. Whether a
                // nullable key makes sense for pagination is the caller's call.
                "Option" => infer(inner(segment)?),
                _ => None,
            }
        }
        _ => None,
    }
}

fn inner(segment: &syn::PathSegment) -> Option<&Type> {
    let syn::PathArguments::AngleBracketed(arguments) = &segment.arguments else {
        return None;
    };

    arguments.args.iter().find_map(|argument| match argument {
        syn::GenericArgument::Type(ty) => Some(ty),
        _ => None,
    })
}
