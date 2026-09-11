HRNS_BIN := $(PWD)/target/debug/hrns

# TARGET selects a cross-compilation triple; unset builds for the host.
CARGO_TARGET := $(if $(TARGET),--target $(TARGET))

build:
	cargo build --workspace
.PHONY: build

build-release:
	cargo build --workspace --release --locked $(CARGO_TARGET)
.PHONY: build-release

test:
	cargo test --workspace
.PHONY: test

test-integration: build
	HRNS_BIN=$(HRNS_BIN) python3 ./tests/hrns_integration_test.py
.PHONY: test-integration

lint:
	cargo check --workspace
	cargo clippy --workspace
.PHONY: lint

format:
	cargo fmt
.PHONY: format

precommit: lint test build test-integration
.PHONY: precommit
