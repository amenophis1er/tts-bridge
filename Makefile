.PHONY: check fmt clippy test build docker run

# Run all checks (what CI runs)
check: fmt clippy test

fmt:
	cargo fmt --check

clippy:
	cargo clippy --all-targets -- -D warnings

test:
	cargo test

# Release build
build:
	cargo build --release

# Local Docker image
docker:
	docker build -t tts-bridge .

# Quick local run (plain logs, default backend)
run:
	LOG_FORMAT=plain cargo run
