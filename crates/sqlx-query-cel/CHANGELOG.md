# Changelog

## [1.0.0](https://github.com/sqlx-contrib/sqlx-query/compare/sqlx-query-cel-v0.1.0...sqlx-query-cel-v1.0.0) (2026-09-24)


### ⚠ BREAKING CHANGES

* `FilterClauseError::Blank` is gone; `FilterClause::parse` returns `Ok` for a blank string.
* `Value` gains a `Uuid` variant and `FilterClauseError` an `InvalidLiteral` one; an exhaustive match on either needs the new arm.
* make the drivers and the date library cargo features
* number placeholders internally so MySQL and SQLite bind correctly ([#3](https://github.com/sqlx-contrib/sqlx-query/issues/3))

### Features

* a blank filter is the empty filter, not an error ([#8](https://github.com/sqlx-contrib/sqlx-query/issues/8)) ([7dcfa8e](https://github.com/sqlx-contrib/sqlx-query/commit/7dcfa8ee9dc197e9155a255b38f22e624a5f4e87))
* make the drivers and the date library cargo features ([f1fdc12](https://github.com/sqlx-contrib/sqlx-query/commit/f1fdc126910522f0d0a6bfab2f6b70d632a5642d))
* number placeholders internally so MySQL and SQLite bind correctly ([#3](https://github.com/sqlx-contrib/sqlx-query/issues/3)) ([b716f04](https://github.com/sqlx-contrib/sqlx-query/commit/b716f04372952d40d209f0aa8563900605cb193f))
* string matching, timestamp() and uuid() in filters, and cursors on UUID keys ([#5](https://github.com/sqlx-contrib/sqlx-query/issues/5)) ([5b7bd74](https://github.com/sqlx-contrib/sqlx-query/commit/5b7bd74165a9dcfc6737e77862685865cf0d6de1))


### Dependencies

* The following workspace dependencies were updated
  * dependencies
    * sqlx-query bumped from 0.1.0 to 1.0.0
