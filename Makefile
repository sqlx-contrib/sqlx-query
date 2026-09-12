# Run these inside the dev shell -- `nix develop`, or a Dev Container, which
# gives you the same toolchain and the same two database servers. The commands
# are bare rather than wrapped in `nix develop --command` because that
# re-evaluates the flake every time, which costs a couple of seconds per target
# and nests a second shell inside the one you are probably already in.
#
# Outside the shell everything still runs: the PostgreSQL and MySQL tests read
# SQLX_QUERY_POSTGRES_URL and SQLX_QUERY_MYSQL_URL and skip when they are unset,
# so `make test` on a machine with no Docker is green -- and covers less. `make
# databases` says which of the two are actually running.

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

# Each driver on its own, with and without CEL. Consumers take this crate with
# one driver and no default features, and that configuration has broken twice
# while the all-features build stayed green.
.PHONY: lint-features
lint-features:
	@for features in postgres sqlite mysql postgres,cel sqlite,cel mysql,cel; do \
		echo "--- $$features"; \
		cargo clippy --quiet --all-targets --no-default-features --features $$features || exit 1; \
	done

# Which databases the tests will actually reach. Prints nothing useful outside
# the dev shell, which is itself the answer.
.PHONY: databases
databases:
	@echo "SQLX_QUERY_POSTGRES_URL = $${SQLX_QUERY_POSTGRES_URL:-(unset -- PostgreSQL tests will skip)}"
	@echo "SQLX_QUERY_MYSQL_URL    = $${SQLX_QUERY_MYSQL_URL:-(unset -- MySQL tests will skip)}"

.PHONY: doc
doc:
	cargo doc --all-features --no-deps --open
