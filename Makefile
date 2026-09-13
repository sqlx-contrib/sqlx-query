# Run these inside the dev shell -- `nix develop`, or a Dev Container, which
# gives you the same toolchain and the same two database servers. The commands
# are bare rather than wrapped in `nix develop --command` because that
# re-evaluates the flake every time, which costs a couple of seconds per target
# and nests a second shell inside the one you are probably already in.
#
# Outside the shell everything still runs: the PostgreSQL and MySQL tests read
# SQLX_QUERY_POSTGRES_URL and SQLX_QUERY_MYSQL_URL and skip when they are unset,
# so `make test` on a machine with no Docker is green -- and covers less.

# Every driver and the CEL filter language. A test that names a driver is gated
# on its feature, so a narrower set silently runs fewer tests rather than
# failing.
FEATURES := postgres,sqlite,mysql,cel

.PHONY: test
test:
	cargo test --all-targets --features $(FEATURES)
	cargo test --doc --features $(FEATURES)

.PHONY: lint
lint:
	cargo fmt --all --check
	cargo clippy --all-targets --features $(FEATURES)

.PHONY: doc
doc:
	cargo doc --all-features --no-deps --open
