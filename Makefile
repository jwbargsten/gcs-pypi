.PHONY: build test fmt lint dist clean help

.DEFAULT_GOAL := help

COMMIT_ID=$(shell git rev-parse --short=8 HEAD 2>/dev/null || echo dev)
DIST_TARGET ?= x86_64-unknown-linux-musl

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

clean: ## clean and remove dist dir
	cargo clean
	rm -rf dist

help: ## this help
	@awk 'BEGIN {FS = ":.*?## "} /^[a-zA-Z0-9._-]+:.*?## / {printf "\033[1m\033[36m%-38s\033[0m %s\n", $$1, $$2}' $(MAKEFILE_LIST)
