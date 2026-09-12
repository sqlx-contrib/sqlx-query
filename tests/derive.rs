//! `#[derive(Schema)]` declares the allow-list once, on the struct.
#![cfg(all(feature = "derive", feature = "postgres"))]

use chrono::{DateTime, Utc};
use sqlx_query::{ColumnType, Schema as _};

// The derive reads the fields; nothing here constructs one.
#[allow(dead_code)]
#[derive(sqlx_query::Schema)]
#[schema(rename_all = "camelCase")]
struct Volume {
    #[schema(key)]
    id: i64,
    title: String,
    read_count: i64,
    price: f64,
    archived: bool,
    cover: Vec<u8>,
    published_at: DateTime<Utc>,
    #[schema(column = "author_name")]
    author: String,
    #[schema(rename = "tag", column = "tag_label")]
    label: Option<String>,
    #[schema(ty = "text")]
    opaque: SomethingCustom,
    #[schema(skip)]
    internal_note: String,
}

struct SomethingCustom;

#[test]
fn rename_all_maps_fields_to_their_request_names() {
    let schema = Volume::schema();

    assert_eq!(schema.resolve(&["readCount"]).unwrap().name, "read_count");
    assert_eq!(
        schema.resolve(&["publishedAt"]).unwrap().name,
        "published_at"
    );

    // The Rust spelling is not exposed; only the request-facing one is.
    assert!(schema.resolve(&["read_count"]).is_none());
}

#[test]
fn a_column_may_be_spelled_differently_from_its_field() {
    let schema = Volume::schema();

    assert_eq!(schema.resolve(&["author"]).unwrap().name, "author_name");

    // `rename` names the request side, `column` the database side.
    assert_eq!(schema.resolve(&["tag"]).unwrap().name, "tag_label");
    assert!(schema.resolve(&["label"]).is_none());
}

#[test]
fn only_key_marks_a_column_unique() {
    let schema = Volume::schema();

    assert!(schema.resolve(&["id"]).unwrap().unique);
    assert!(!schema.resolve(&["title"]).unwrap().unique);
}

#[test]
fn types_are_inferred_from_the_rust_type() {
    let schema = Volume::schema();

    assert_eq!(schema.resolve(&["id"]).unwrap().ty, ColumnType::Int);
    assert_eq!(schema.resolve(&["title"]).unwrap().ty, ColumnType::Text);
    assert_eq!(schema.resolve(&["price"]).unwrap().ty, ColumnType::Float);
    assert_eq!(schema.resolve(&["archived"]).unwrap().ty, ColumnType::Bool);
    assert_eq!(schema.resolve(&["cover"]).unwrap().ty, ColumnType::Bytes);
    assert_eq!(
        schema.resolve(&["publishedAt"]).unwrap().ty,
        ColumnType::Timestamp
    );
}

/// A nullable column is still that column's type.
#[test]
fn option_unwraps_to_its_inner_type() {
    assert_eq!(
        Volume::schema().resolve(&["tag"]).unwrap().ty,
        ColumnType::Text
    );
}

/// The escape hatch, for a type this crate cannot guess.
#[test]
fn an_explicit_type_overrides_inference() {
    assert_eq!(
        Volume::schema().resolve(&["opaque"]).unwrap().ty,
        ColumnType::Text
    );
}

/// Fail-closed: a skipped field is not filterable or sortable at all.
#[test]
fn a_skipped_field_is_not_exposed() {
    assert!(Volume::schema().resolve(&["internalNote"]).is_none());
    assert!(Volume::schema().resolve(&["internal_note"]).is_none());
}

/// Without `rename_all`, the field is exposed as Rust spells it.
#[allow(dead_code)]
#[derive(sqlx_query::Schema)]
struct Plain {
    #[schema(key)]
    id: i64,
    read_count: i64,
}

#[test]
fn the_default_is_the_field_name_as_written() {
    assert_eq!(
        Plain::schema().resolve(&["read_count"]).unwrap().name,
        "read_count"
    );
    assert!(Plain::schema().resolve(&["readCount"]).is_none());
}
