.PHONY: build test lint check clean install dev release

build:
	cargo build --workspace

release:
	cargo build --workspace --release

test:
	cargo test --workspace

lint:
	cargo fmt --all -- --check
	cargo clippy --workspace -- -D warnings

check:
	cargo check --workspace

clean:
	cargo clean

install:
	cargo install --path crates/oxidepm
	cargo install --path crates/oxidepmd
	cargo install --path crates/oxidepm-sandbox

dev:
	cargo watch -x 'check --workspace'

fmt:
	cargo fmt --all
