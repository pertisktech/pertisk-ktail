.PHONY: build release test clean fmt lint

build:
	cargo build

release:
	cargo build --release

test:
	cargo test

clean:
	cargo clean

fmt:
	cargo fmt

lint:
	cargo clippy -- -D warnings

check:
	cargo check

run:
	cargo run --

help:
	@echo "Available targets:"
	@echo "  build      - Build debug binary"
	@echo "  release    - Build optimized release binary"
	@echo "  test       - Run tests"
	@echo "  clean      - Remove build artifacts"
	@echo "  fmt        - Format code"
	@echo "  lint       - Run clippy linter"
	@echo "  check      - Check code without building"
	@echo "  run        - Run the binary"
