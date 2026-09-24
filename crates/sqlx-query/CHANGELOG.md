# Changelog

## [1.0.0](https://github.com/sqlx-contrib/sqlx-query/compare/sqlx-query-v0.1.0...sqlx-query-v1.0.0) (2026-09-24)


### ⚠ BREAKING CHANGES

* `Page::cursor` is `Cursor`, not `Option<Cursor>`: test `is_empty()` for the last page. `Cursor::parse("")` returns `Ok` with the empty cursor rather than a token error.
* `FilterClauseError::Blank` is gone; `FilterClause::parse` returns `Ok` for a blank string.
* `Value` gains a `Uuid` variant and `FilterClauseError` an `InvalidLiteral` one; an exhaustive match on either needs the new arm.
* make the drivers and the date library cargo features
* bind any type sqlx can encode, not just Value's seven scalars
* number placeholders internally so MySQL and SQLite bind correctly ([#3](https://github.com/sqlx-contrib/sqlx-query/issues/3))

### Features

* a blank filter is the empty filter, not an error ([#8](https://github.com/sqlx-contrib/sqlx-query/issues/8)) ([7dcfa8e](https://github.com/sqlx-contrib/sqlx-query/commit/7dcfa8ee9dc197e9155a255b38f22e624a5f4e87))
* a Pager that cuts a keyset page and the cursor to the next ([#7](https://github.com/sqlx-contrib/sqlx-query/issues/7)) ([129d8e6](https://github.com/sqlx-contrib/sqlx-query/commit/129d8e6bfd037aec977e496fbb506d28f42c8b03))
* bind any type sqlx can encode, not just Value's seven scalars ([1c430d4](https://github.com/sqlx-contrib/sqlx-query/commit/1c430d43640ba9518c490e1c42960b9f69261fcc))
* build an order_by without parsing, and combine a where with OR ([eb01764](https://github.com/sqlx-contrib/sqlx-query/commit/eb017648c60ce13c6ba4d510628a810c3503ddc8))
* make the drivers and the date library cargo features ([f1fdc12](https://github.com/sqlx-contrib/sqlx-query/commit/f1fdc126910522f0d0a6bfab2f6b70d632a5642d))
* number placeholders internally so MySQL and SQLite bind correctly ([#3](https://github.com/sqlx-contrib/sqlx-query/issues/3)) ([b716f04](https://github.com/sqlx-contrib/sqlx-query/commit/b716f04372952d40d209f0aa8563900605cb193f))
* string matching, timestamp() and uuid() in filters, and cursors on UUID keys ([#5](https://github.com/sqlx-contrib/sqlx-query/issues/5)) ([5b7bd74](https://github.com/sqlx-contrib/sqlx-query/commit/5b7bd74165a9dcfc6737e77862685865cf0d6de1))
* the empty cursor is the empty page token, both ways ([#9](https://github.com/sqlx-contrib/sqlx-query/issues/9)) ([9b83709](https://github.com/sqlx-contrib/sqlx-query/commit/9b83709382e7908429e47339380354119758e6ac))


### Bug Fixes

* parenthesise the where clause the composer splices in ([#6](https://github.com/sqlx-contrib/sqlx-query/issues/6)) ([98aae2d](https://github.com/sqlx-contrib/sqlx-query/commit/98aae2df8b3cba768f05228e963b184351e783bb))
* refuse an order_by field that is not an identifier ([5c1bef0](https://github.com/sqlx-contrib/sqlx-query/commit/5c1bef08a3d79f1a0005d79d56551e679aa3c5ac))
* **test:** skip the live-driver tests on unreachable, not on unset ([8312b1a](https://github.com/sqlx-contrib/sqlx-query/commit/8312b1a436507533696b54b1ec23d95abb267e16))
