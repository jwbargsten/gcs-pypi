.PHONY: build test fmt lint dist wheel clean help

.DEFAULT_GOAL := help

COMMIT_ID=$(shell git rev-parse --short=8 HEAD 2>/dev/null || echo dev)
DIST_TARGET ?= x86_64-unknown-linux-musl
WHEEL_TARGET ?= x86_64-unknown-linux-gnu
MATURIN_VERSION = 1.15.0
ZIGLANG_VERSION = 0.16.0

build: ## debug build
	cargo build

test: ## run tests
	cargo test

fmt: ## apply clippy fixes and format
	cargo clippy --fix --allow-dirty
	cargo fmt

lint: ## check formatting and lints, fail on warnings (for use in e.g. ci/cd)
	cargo fmt --check
	cargo clippy --all-targets -- -D warnings

dist: clean lint test ## lint, test and build optimized binary into ./dist (override target with DIST_TARGET)
	cargo build --release --target $(DIST_TARGET)
	mkdir -p dist
	cp target/$(DIST_TARGET)/release/gcs-pypi dist/gcs-pypi-$(COMMIT_ID)-$(DIST_TARGET)

wheel: clean lint test ## lint, test and build the manylinux wheel into ./dist (override target with WHEEL_TARGET)
	uvx --with ziglang==$(ZIGLANG_VERSION) maturin@$(MATURIN_VERSION) build --release --zig \
		--target $(WHEEL_TARGET) --compatibility manylinux2014 --out dist

clean: ## clean and remove dist dir
	cargo clean
	rm -rf dist

help: ## this help
	@awk 'BEGIN {FS = ":.*?## "} /^[a-zA-Z0-9._-]+:.*?## / {printf "\033[1m\033[36m%-38s\033[0m %s\n", $$1, $$2}' $(MAKEFILE_LIST)
