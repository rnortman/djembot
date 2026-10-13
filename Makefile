.PHONY: check rust-check init-check msrv fmt clippy test scrub-tree

# Full pre-commit check suite. CI's check job runs rust-check; its scrub job
# runs `brenn-scrub init --check` and the tree scan through brenn-scrub's
# tree-scan.yml.
check: rust-check init-check

rust-check: msrv fmt clippy test

# rust-version claims only the pinned toolchain: it must equal the
# major.minor of rust-toolchain.toml's channel.
msrv:
	@claim=$$(sed -n 's/^rust-version *= *"\(.*\)"/\1/p' Cargo.toml); \
	pin=$$(sed -n 's/^channel *= *"\([0-9]*\.[0-9]*\).*"/\1/p' rust-toolchain.toml); \
	if [ -z "$$claim" ] || [ "$$claim" != "$$pin" ]; then \
		echo "msrv: Cargo.toml rust-version '$$claim' != rust-toolchain.toml channel major.minor '$$pin'" >&2; \
		exit 1; \
	fi

fmt:
	cargo fmt -- --check

clippy:
	cargo clippy --all-targets -- -D warnings

test:
	cargo test --locked

# The scrub kit (hook shims, .gitleaks.toml rules, overlay gitignore line,
# core.hooksPath) still matches what the installed brenn-scrub expects.
init-check:
	brenn-scrub init --check

scrub-tree:
	brenn-scrub tree
