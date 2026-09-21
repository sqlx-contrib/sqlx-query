# Run these inside the dev shell -- `nix develop`, or a Dev Container, which
# gives you the same toolchain. The commands are bare rather than wrapped in
# `nix develop --command` because that re-evaluates the flake every time, which
# costs a couple of seconds per target and nests a second shell inside the one
# you are probably already in.
#
# Nothing here needs a database server: the tests compare the SQL these crates
# render, so `make test` is the same on a laptop as it is in CI.

.PHONY: test
test:
	cargo test --workspace --all-targets
# A second run, because `--all-targets` silently excludes doctests -- there are
# none yet, so this is what keeps the first example added to a doc comment from
# going unrun. Unindented, so make eats the comment instead of the shell
# echoing it.
	cargo test --workspace --doc

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
