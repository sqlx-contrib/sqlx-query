# Run these inside the dev shell -- `nix develop`, or a Dev Container, which
# gives you the same toolchain. The commands are bare rather than wrapped in
# `nix develop --command` because that re-evaluates the flake every time, which
# costs a couple of seconds per target and nests a second shell inside the one
# you are probably already in.
#
# `make test` needs no server. SQLite runs in memory, so the live-driver tests
# for it run everywhere; the PostgreSQL and MySQL ones skip themselves unless
# their URL is set. `make test-servers` starts both and runs them.

.PHONY: test
test:
	cargo test --workspace --all-targets
# A second run, because `--all-targets` silently excludes doctests -- there are
# none yet, so this is what keeps the first example added to a doc comment from
# going unrun. Unindented, so make eats the comment instead of the shell
# echoing it.
	cargo test --workspace --doc

# The same suite with PostgreSQL and MySQL actually running. Inside the Dev
# Container the URLs are already set, so `make test` covers everything and this
# is only needed from a host.
.PHONY: test-servers
test-servers:
	docker compose -f .devcontainer/docker-compose.yml -p sqlx-query-test up -d --wait postgres mysql
	SQLX_QUERY_POSTGRES_URL="postgres://vscode@localhost:$$(docker compose -f .devcontainer/docker-compose.yml -p sqlx-query-test port postgres 5432 | cut -d: -f2)/sqlx_query" \
	SQLX_QUERY_MYSQL_URL="mysql://root@localhost:$$(docker compose -f .devcontainer/docker-compose.yml -p sqlx-query-test port mysql 3306 | cut -d: -f2)/sqlx_query" \
	cargo test --workspace --all-targets

.PHONY: test-servers-down
test-servers-down:
	docker compose -f .devcontainer/docker-compose.yml -p sqlx-query-test down -v

.PHONY: lint
lint:
	cargo fmt --all --check
	cargo clippy --workspace --all-targets

.PHONY: doc
doc:
	cargo doc --workspace --no-deps --open

# What CI runs, and what `doc` deliberately isn't: no browser, and warnings are
# errors -- a broken intra-doc link fails the build instead of waiting to be
# noticed. This API is documented by cross-reference, so a rename that outruns
# its links is the likeliest way for the docs to go wrong.
.PHONY: doc-check
doc-check:
	RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps
