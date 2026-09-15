# Run these inside the dev shell -- `nix develop`, or a Dev Container, which
# gives you the same toolchain. The commands are bare rather than wrapped in
# `nix develop --command` because that re-evaluates the flake every time, which
# costs a couple of seconds per target and nests a second shell inside the one
# you are probably already in.
#
# Nothing here needs a database server. The end-to-end tests run against SQLite
# in memory, so `make test` is the same on a laptop as it is in CI.

# Every driver, and the CEL filter language. A test that names one is gated on
# its feature, so a narrower set silently runs fewer tests rather than failing.
FEATURES := postgres,sqlite,cel

.PHONY: test
test:
	cargo test --all-targets --features $(FEATURES)
	cargo test --doc --features $(FEATURES)

# Every driver alone, with and without CEL, and then all of them together.
# Consumers take this crate with one driver and no default features, and that
# configuration is easy to break while the all-features build stays green -- so
# it is linted by default rather than on request.
DRIVERS := postgres sqlite postgres,cel sqlite,cel

.PHONY: lint
lint:
	cargo fmt --all --check
	cargo clippy --all-targets --features $(FEATURES)
	@for features in $(DRIVERS); do \
		echo "--- $$features"; \
		cargo clippy --quiet --all-targets --no-default-features --features $$features || exit 1; \
	done

.PHONY: doc
doc:
	cargo doc --all-features --no-deps --open
